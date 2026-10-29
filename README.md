# ln-payments-plugin — WIP, do not use

Lightning payments for mesh-llm as a plugin: the `pay-lightning` utility plus the
payment engine crates it is built from. Moved from `Mesh-LLM/mesh-llm`
(`mesh/crates/`) at `a4cb198d0` (PR #2654 branch `jimmy/lightning-wallet-library`).

| crate | owns |
|---|---|
| `mesh-llm-wallet` | `wallet.v1` contract and `plugin-server` for wallet plugins |
| `mesh-llm-payments-types` | wire + ledger types (`pay.ln.v1`) |
| `mesh-llm-payments` | ledger, pricing/policy, metered credit, settlement |
| `mesh-llm-pay-lightning` | the plugin binary (`pay-lightning`) |

mesh-llm is a git dependency (`mesh-llm-plugin`, `mesh-llm-events`); the host links none of these.
Wallet plugins (lexe, nwc, ldk) are separate repositories and are unchanged.

See `crates/mesh-llm-pay-lightning/README.md` for the plugin's configuration and packaging.
