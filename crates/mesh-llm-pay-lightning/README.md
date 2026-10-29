# mesh-llm-pay-lightning

Standalone Lightning payments utility and its admission/settlement engine. Mesh
owns generic authenticated transport, admission hooks and authoritative terminal
usage; this utility owns pricing, policy, durable ledger, tickets, invoices and
reconciliation. A separately installed `wallet.v1` plugin owns provider-specific
wallet access. The host does not link this crate, the ledger, payment types or
wallet abstraction. Payment operations are not MCP tools.

Both peers install `pay-lightning`, communicating on `pay.ln.v1`. Their wallet
plugins may have different names; existing compatible wallet binaries need not
be rebuilt. `run(name, version, factory, config)` remains for in-process fixtures;
the installed executable uses `run_bound` and the host-owned wallet connection.

## Zero-config install

`mesh-llm plugins install <pay-lightning archive>` plus one installed `wallet.v1`
plugin (for example `Mesh-LLM/lexe-wallet`, unchanged) is the whole setup, as it
was when payments were built into mesh-llm:

- The package manifest asks the host for a `wallet.v1` binding named `wallet`,
  the exchange hook and peer-block permission. Installing is the consent; the
  host grants them without any `[[plugin]]` stanza.
- The host binds the single running wallet plugin on first use and stays with it.
  Two running wallet plugins are refused until the operator names one (below).
- With no settings the utility uses `~/.mesh-llm/payments/`: `payment.json`
  (created with non-spending defaults), the ledger, `wallet-provider.json` and
  `wallets/<wallet plugin>/` as the wallet's data directory — the same layout as
  built-in payments, so an existing wallet is found again. First wallet
  adoption is allowed (`allow_wallet_provisioning` defaults to `true`).
- `mesh-llm wallet balance|fund|send|transactions|policy|pricing|…` forward to
  this plugin's published `control` operation over the node's local API.

## Explicit operator binding (optional)

A `[[plugin]]` entry overrides any of the above: the three settings in
`MESH_LLM_PLUGIN_SETTINGS_JSON`, and a named provider binding:

```toml
[[plugin]]
name = "pay-lightning"
settings = { config_path = "/absolute/payment/config.json", wallet_binding = "wallet", allow_wallet_provisioning = false }
allow_peer_blocks = true

[[plugin.operation_bindings]]
name = "wallet"
provider = "lexe-wallet"
profile = "default"
setup_operation = "wallet_open"
setup_input_json = '{"directory":"/absolute/existing/wallet-state"}'
operations = ["wallet_balance", "wallet_transactions", "wallet_create_invoice", "wallet_pay", "wallet_lookup", "wallet_wait_for_payment", "wallet_wait_for_arrival"]
```

A named binding exclusively selects one provider and fixes the open payload; the
utility then cannot supply an open directory. The host alone launches and
supervises the wallet; killing the utility does not kill that wallet.

`config_path` points to a separate payment JSON file:

```json
{
  "data_dir": "/absolute/payment/ledger",
  "prices": {}, "buy_prices": {}, "buy_peers": [], "cap_msat": 0,
  "policy": {"mode": "free_only"}
}
```

The utility pins `wallet_id`, `provider` and `network` in the ledger directory
before starting its engine. A changed identity or non-mainnet wallet is refused.
The pin's historical `plugin` field records the **provider binding name**. First
adoption without a ledger pin requires `allow_wallet_provisioning = true`, before
any setup is attempted; this also applies when adopting an already provisioned
wallet into a fresh ledger. Installing a package or inspecting `--manifest`
never opens a wallet. Do not copy or replace funded wallet state to change a pin.

Setup tolerates provider-startup ordering by retrying only requests that the host
reports were not dispatched, for a bounded 90-second window. A provider error or
uncertain setup fails closed. Repeated successful setup must be idempotent. A
provider restart invalidates the utility's handle; the utility then re-runs
setup once (re-checking the pinned wallet identity) and retries the refused
operation, including `wallet_pay`, because the host refused it before anything
reached the provider. A second refusal fails closed.

`prices` maps model IDs to `input_msat_per_million`, `output_msat_per_million` and
`cache_discount_percent`. Buying is decided by the ledger policy alone, as it was
for built-in payments: `policy.mode = "automatic"` with daily budget left
(`mesh-llm wallet policy --mode automatic --daily-budget-sats N`); the budget is
daily, not a per-request cap, and the admission's `max_total_msat` is that
budget. The static fields only narrow it: `buy_peers` (exact authenticated peer
IDs) restricts which sellers are paid, `buy_prices` (same shape as `prices`)
caps a seller's per-model rates, `cap_msat` when non-zero lowers the admission
cap. The handshake starts when a request selects a seller; a seller listed in
`buy_peers` is handshaken eagerly on peer-up. Unsolicited admissions are refused.
Defaults (`free_only`) never authorize spending. Optional absolute `timing_log`
and `error_log` paths belong inside isolated process-owned state.

The host must grant required metadata/admission access to `request_received`,
`backend_selected`, `exchange_finished`, header
`x-plugin-7061792d6c696768746e696e67-pay-terms`, a 30000 ms deadline and peer block
permission. See the host's exchange contract,
[`mesh/docs/plugins/openai-exchange-lifecycle.md`](https://github.com/Mesh-LLM/mesh-llm/blob/eba2a8ecf/mesh/docs/plugins/openai-exchange-lifecycle.md),
and the operator-bound operations section of
[`mesh/docs/plugins/README.md`](https://github.com/Mesh-LLM/mesh-llm/blob/eba2a8ecf/mesh/docs/plugins/README.md)
at the pinned mesh-llm revision.

## Admission, settlement and lifecycle

Cold clients pay a deposit. Warm tickets refill concurrently with inference,
without waiting for the final wallet payment. The three-unpaid-final limit still
bounds credit. An independent receipt sweep wakes admission even when a buyer's
`Paid` message is delayed. Persisted invoice receipts reconcile after restart.
Background receipt observation reserves at most eight invoices per sweep, with
one-second lookup timeouts and per-invoice exponential backoff from 250 ms to
60 seconds. Attempts are reserved before I/O so cancellation cannot hot-loop the
same first invoice. Expired unpaid invoices remain debts and continue low-rate
observation; expiry is never payment or forgiveness. This bounds each sweep's
wallet I/O, not the ledger scan or total scheduler memory.

Uncertain payments are reconciled by hash, never blindly replayed by the adapter.
A request known not to have been dispatched is classified as not submitted;
transport ambiguity remains uncertain. Long-poll observations can be reissued
after the host's binding deadline; that does not retry a payment. One outstanding
ticket per buyer/model is supported; restart abandons interrupted work and drops
unconsumed tickets rather than replaying inference.

## Offline development and evidence boundaries

Run `just check`, `just test`, `just build`, `just package` from the repository
root. `package` stages the installable utility under `dist/`; the tests run with
`--all-features` so the `offline-mock` fixtures are included, serially. Raise
the macOS file limit with `ulimit -n 8192` before the full suite. The opt-in
`offline-mock` binary uses signed synthetic invoices and an explicit shared file
network. It never contacts wallet services or moves funds. Its
`MESH_LLM_OFFLINE_WALLET_CHILD` mode serves only `wallet.v1` for legacy subprocess
adapter tests; the installed utility does not use that child adapter, and the
`wallet_child` module is compiled only with that feature.

Tests cover synthetic payment settlement/recovery and legacy child ownership,
plus the installed utility's host RPC setup, startup delay, SIGKILL and identity
pin reuse. Synthetic children are one backend under different names, not two
independently implemented real providers. These tests do not establish live
QUIC/inference performance, automatic install/binding, wallet setup UX, migration,
removal UX or cross-platform installed behavior. Remove `target/` and `dist/`
after validation.
