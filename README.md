# ln-payments-plugin — WIP, do not use

Lightning payments for mesh-llm as an installable plugin, `pay-lightning`, plus
the payment-engine crates it is built from. Moved out of `Mesh-LLM/mesh-llm`
(`mesh/crates/`, PR [#2654](https://github.com/Mesh-LLM/mesh-llm/pull/2654),
branch `jimmy/lightning-wallet-library`). mesh-llm keeps only the generic
seams — operation bindings, OpenAI-exchange hooks and the `mesh-llm wallet` CLI
forward — and links none of these crates.

**Work in progress.** Not released, not in the plugin catalog, no compatibility
with the built-in payment path in released mesh-llm. A zero-config paid
baseline has been proven live once (see [Status](#status-2026-10-29)); that is a
baseline, not a readiness claim.

| crate | owns |
|---|---|
| `mesh-llm-wallet` | the `wallet.v1` contract and `plugin-server` used by wallet plugins |
| `mesh-llm-payments-types` | wire + ledger types (`pay.ln.v1`) |
| `mesh-llm-payments` | ledger, pricing/policy, metered credit, settlement |
| `mesh-llm-pay-lightning` | the plugin binary (`pay-lightning`) — [crate README](crates/mesh-llm-pay-lightning/README.md) |

mesh-llm SDK crates (`mesh-llm-plugin`, `mesh-llm-events`,
`mesh-llm-plugin-manager`) are git dependencies pinned to #2654 commit
`eba2a8ecf`. Wallet plugins (`Mesh-LLM/lexe-wallet`, nwc, ldk) are separate
repositories and are unchanged.

## How it works

- **Install is the consent.** The package manifest declares one `wallet.v1`
  capability binding named `wallet`, the OpenAI-exchange hook
  (`request_received`, `backend_selected`, `exchange_finished`, the
  `x-plugin-…-pay-terms` response header) and peer-block permission.
  `mesh-llm plugins install` prints those grants and applies them; no
  `[[plugin]]` stanza is needed. A `[[plugin]]` entry can still override any of
  it (see the crate README).
- **The wallet is reached through the host, never linked.** On first use the
  host resolves the `wallet.v1` binding to the single running wallet plugin and
  pins it; every wallet call (`wallet_balance`, `wallet_create_invoice`,
  `wallet_pay`, …) is a `plugin/bound_operation` RPC with a host-minted handle.
  One binding owns one provider; two running wallet plugins are refused until
  the operator names one.
- **Seller.** `mesh-llm wallet pricing <model> --input-msat-per-million …
  --output-msat-per-million …` prices a model. A request for a priced model
  without a valid ticket is answered `402 Payment Required` with the pay-terms
  header; a buyer that negotiates over `pay.ln.v1` pays a deposit, inference
  runs, and the final bill is settled from the backend-reported usage.
- **Buyer.** Spends under the ledger policy only —
  `mesh-llm wallet policy --mode automatic --daily-budget-sats N` — the same
  rule built-in payments used; the handshake starts when a request selects
  that seller. The default policy is `free_only`, so a fresh install buys
  nothing and gets the 402.
- **State** lives in `~/.mesh-llm/payments/`: `payment.json` (created with
  non-spending defaults), the ledger, `wallet-provider.json` and
  `wallets/<wallet plugin>/` — the layout built-in payments used, so an
  existing wallet is found again.
- **CLI.** `mesh-llm wallet balance|fund|send|transactions|pending|policy|
  pricing|…` forward to the plugin's published `control` operation over the
  node's local API.

## Build, package, install

Needs a mesh-llm built from #2654 (`eba2a8ecf` or later on that branch);
released mesh-llm has none of the seams.

```sh
just check                # fmt + clippy
just test                 # serial; the suite is flaky in parallel
just package              # dist/pay-lightning-v<version>-<target>.tar.gz

mesh-llm plugins install Mesh-LLM/lexe-wallet
mesh-llm plugins install --archive dist/pay-lightning-v0.78.0-<target>.tar.gz \
    --name pay-lightning --version 0.78.0
```

Install **both** plugins, in either order, before starting the node. The host
resolves `pay-lightning`'s `wallet.v1` binding to a running wallet plugin; it
does not install one, and installing a wallet does not install `pay-lightning`.
Without a wallet plugin `pay-lightning` starts but every wallet call is refused
(`no running plugin serves wallet.v1`).

Then on the seller `mesh-llm --model <model> --console <port>` and
`mesh-llm wallet --port <console port> pricing <model> …`; on the buyer
`mesh-llm client --join <token> --console <port>` and
`mesh-llm wallet --port <console port> policy --mode automatic --daily-budget-sats 600`.
Nothing else is written by hand.

## Status (2026-10-29)

Proven at mesh-llm `eba2a8ecf` + this repo `bf8aee0`, two fresh `$HOME`s on
one machine, `lexe-wallet` v0.1.0 as released, zero config; evidence
independently re-derived by a second reviewer:

- **Paid:** 5×`200`; five new buyer payments of 1,000 msat and five new seller
  receipts of 995 msat with the same payment hashes, all inside the request
  window (before/after transaction lists on both wallets).
- **Refused:** buyer left at `free_only` → 5×`402` carrying the pay-terms
  header, zero new transactions on either wallet.
- `just test`: 47 unit + 4 integration tests.

Known gaps, none of them signed off:

- `mesh-llm wallet unpin` is unsupported in the plugin shape (it needs a
  stopped-node path the plugin does not have).
- The seller's brand-new Lexe wallet reported `spendable_msat: 0` after five
  settled inbound payments — unresolved, not attributed.
- The test suite is flaky in parallel; `just test` runs it serially.
- Not in the plugin catalog; `pay-lightning` is both the installed plugin name
  and the protocol slot — naming is under review.
- Install UX: a wallet install does not yet bring `pay-lightning` with it.
  Making `mesh-llm plugins install Mesh-LLM/lexe-wallet` sufficient needs
  mesh-llm #2687 (`plugin.toml` `requires = [...]`, install fetches a missing
  requirement from the catalog) merged, a lexe-wallet release declaring
  `requires = ["pay-lightning"]`, and `pay-lightning` published in the catalog
  so the installer can resolve it by name. Until then, install both by hand.
- mesh-llm #2654 still carries copies of these four crates in its workspace;
  removing them and updating `mesh/docs/specs/lightning-payments.md` (still the
  built-in protocol text) are pending on that PR.
