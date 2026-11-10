# ln-payments-plugin

Lightning payments for mesh-llm. The `pay-lightning` plugin handles model
pricing, spending policy, deposits and settlement. A separate wallet plugin
handles Lightning payments.

Micropayments in mesh-llm are in developer preview—use with caution and small
amounts only. Use your own wallet and funds.

Binary packages are available on
[GitHub Releases](https://github.com/Mesh-LLM/ln-payments-plugin/releases).
Both buyer and seller need `pay-lightning` and a compatible wallet plugin.

## Install

Requires mesh-llm with plugin payment support (`b63cea23c` or later;
v0.78.0 and earlier are not supported).

Install `pay-lightning` with **one** wallet provider:

```sh
mesh-llm plugins install Mesh-LLM/lexe-wallet-plugin Mesh-LLM/ln-payments-plugin
# Alternatively:
mesh-llm plugins install Mesh-LLM/nwc-wallet-plugin Mesh-LLM/ln-payments-plugin
```

Install both plugins before starting the node. The host connects `pay-lightning`
to the running wallet provider. Without a wallet provider, payment operations
are unavailable. If multiple wallet providers are running, select one explicitly
in the [plugin configuration](crates/mesh-llm-pay-lightning/README.md#explicit-operator-binding-optional).

## Sell inference

Start `mesh-llm --model <model>`, then set the model's prices in millisatoshis
per million tokens:

```sh
mesh-llm plugins call pay-lightning control --input \
  '{"command":"set_pricing","model":"<model>","value":{"input_msat_per_million":1000,"output_msat_per_million":1000}}'
```

Buyers pay a deposit before inference. The final bill uses backend-reported
usage. Requests without a valid payment ticket receive `402 Payment Required`.

## Buy inference

Start `mesh-llm client --join <token>`, then enable spending with a daily budget
in millisatoshis:

```sh
mesh-llm plugins call pay-lightning control --input \
  '{"command":"policy","value":{"mode":"automatic","daily_budget_msat":600000}}'
```

The default policy is `free_only`: a fresh install does not authorize spending.
The daily budget is not a per-request limit.

## Wallet and state

Control commands use the running node's API (default port 3131). Add
`--port <api port>` when using a different port.

```sh
mesh-llm plugins call pay-lightning control --input '{"command":"balance"}'
mesh-llm plugins call pay-lightning control --input '{"command":"transactions","limit":20}'
```

Other commands include `fund`, `send`, `pending`, `policy`, `pricing`,
`set_pricing`, `min_payment`, `admission_deposit`, `blocked` and `unblock`.
See [ControlCommand](crates/mesh-llm-payments-types/src/control.rs) for inputs.

State is stored under `~/.mesh-llm/payments/` by default, including configuration,
the ledger, wallet identity and wallet-provider data. Do not replace funded
wallet state to change the pinned wallet identity. The `unpin` command is not
supported by the plugin.

## Development

```sh
just check                # formatting and Clippy
just test                 # full workspace tests, run serially
just package              # release binary and installable archive in dist/
```

To install a local package:

```sh
mesh-llm plugins install --archive dist/pay-lightning-v0.1.2-<target>.tar.gz \
    --name pay-lightning --version 0.1.2
```

| Crate | Purpose |
|---|---|
| `mesh-llm-wallet` | Wallet contract and plugin server |
| `mesh-llm-payments-types` | Payment protocol and ledger types |
| `mesh-llm-payments` | Ledger, pricing, policy and settlement |
| `mesh-llm-pay-lightning` | Plugin executable |

See the [plugin reference](crates/mesh-llm-pay-lightning/README.md) for explicit
wallet bindings, configuration and settlement behavior.
