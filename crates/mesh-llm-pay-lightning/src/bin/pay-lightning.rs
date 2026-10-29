//! Separate utility entrypoint. Runs with no settings at all — the same
//! on-disk layout and defaults mesh-llm used when payments were built in —
//! and takes an operator binding/config only when one is given.
use anyhow::{Context, Result, ensure};
use std::{io::Write, path::PathBuf};

/// Name of the host operation binding the wallet is reached through when the
/// operator names none. The host resolves it to the running `wallet.v1` plugin.
const DEFAULT_WALLET_BINDING: &str = "wallet";

#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Launch {
    /// Payment config file. Default: `<payments dir>/payment.json`, created
    /// with non-spending defaults when absent.
    config_path: Option<PathBuf>,
    wallet_binding: Option<String>,
    /// Whether a ledger without a wallet pin may adopt (or provision) the
    /// bound wallet. Default true: that is what built-in payments did.
    allow_wallet_provisioning: Option<bool>,
}

/// `~/.mesh-llm/payments`: where built-in payments kept the ledger and every
/// wallet plugin's state, so a node moving to the plugin finds its own data.
fn default_payments_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .context("cannot determine the home directory for the payments dir")?;
    Ok(PathBuf::from(home).join(".mesh-llm").join("payments"))
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().any(|arg| arg == "--manifest") {
        writeln!(
            mesh_llm_events::machine_out(),
            "{}",
            mesh_llm_pay_lightning::manifest_json("pay-lightning")?
        )?;
        return Ok(());
    }
    let launch: Launch = match std::env::var("MESH_LLM_PLUGIN_SETTINGS_JSON") {
        Ok(settings) if !settings.trim().is_empty() => serde_json::from_str(&settings)?,
        _ => Launch::default(),
    };
    let config = match launch.config_path {
        Some(path) => {
            ensure!(path.is_absolute(), "config path must be absolute");
            mesh_llm_pay_lightning::Config::load(&path)?
        }
        None => {
            let payments_dir = default_payments_dir()?;
            mesh_llm_pay_lightning::Config::load_or_create(
                &payments_dir.join("payment.json"),
                &payments_dir,
            )?
        }
    };
    let wallet_binding = launch
        .wallet_binding
        .filter(|binding| !binding.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_WALLET_BINDING.to_owned());
    mesh_llm_pay_lightning::run_bound(
        "pay-lightning".into(),
        env!("CARGO_PKG_VERSION"),
        wallet_binding,
        launch.allow_wallet_provisioning.unwrap_or(true),
        config,
    )
    .await
}
