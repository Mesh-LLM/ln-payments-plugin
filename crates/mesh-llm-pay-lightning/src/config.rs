//! Explicit plugin-owned configuration. Loading never opens a wallet or ledger.
use anyhow::{Context, Result, ensure};
use mesh_llm_payments::ledger::Policy;
use mesh_llm_payments_types::pricing::Pricing;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub data_dir: PathBuf,
    #[serde(default)]
    pub prices: BTreeMap<String, Pricing>,
    #[serde(default)]
    pub buy_prices: BTreeMap<String, Pricing>,
    #[serde(default)]
    pub buy_peers: BTreeSet<String>,
    #[serde(default)]
    pub cap_msat: u64,
    #[serde(default)]
    pub policy: Policy,
    #[serde(default)]
    pub error_log: Option<PathBuf>,
    #[serde(default)]
    pub timing_log: Option<PathBuf>,
}
impl Config {
    /// Read a JSON file at an explicitly supplied path. No legacy environment
    /// policy or implicit adoption of an old host payment directory is supported.
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = serde_json::from_slice(
            &std::fs::read(path)
                .with_context(|| format!("read payment config {}", path.display()))?,
        )?;
        config.validate()?;
        Ok(config)
    }
    /// Hosts can pass a config path using the generic plugin environment map.
    /// Otherwise use config.json inside an explicitly selected plugin data dir.
    /// Non-spending defaults rooted at `data_dir`: no prices, no buy
    /// authorizations, `free_only` policy.
    pub fn with_defaults(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            prices: BTreeMap::new(),
            buy_prices: BTreeMap::new(),
            buy_peers: BTreeSet::new(),
            cap_msat: 0,
            policy: Policy::default(),
            error_log: None,
            timing_log: None,
        }
    }

    /// Load `path`, or create it with [`Self::with_defaults`] when it does not
    /// exist. The directory is created owner-only on Unix: the ledger and the
    /// wallet plugins' seeds live under it.
    pub fn load_or_create(path: &Path, data_dir: &Path) -> Result<Self> {
        if path.exists() {
            return Self::load(path);
        }
        ensure!(data_dir.is_absolute(), "payments dir must be absolute");
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("create payments dir {}", data_dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("restrict {}", data_dir.display()))?;
        }
        let config = Self::with_defaults(data_dir.to_path_buf());
        config.validate()?;
        let body = serde_json::to_vec_pretty(&config)?;
        std::fs::write(path, body)
            .with_context(|| format!("write payment config {}", path.display()))?;
        Ok(config)
    }

    pub fn from_launch_env() -> Result<Self> {
        let path = if let Some(path) = std::env::var_os("MESH_LLM_PLUGIN_CONFIG") {
            PathBuf::from(path)
        } else {
            PathBuf::from(
                std::env::var_os("MESH_LLM_PLUGIN_DATA_DIR")
                    .context("set MESH_LLM_PLUGIN_CONFIG or MESH_LLM_PLUGIN_DATA_DIR")?,
            )
            .join("config.json")
        };
        Self::load(&path)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.data_dir.is_absolute(),
            "data_dir must be absolute isolated plugin state"
        );
        for path in [&self.error_log, &self.timing_log].into_iter().flatten() {
            ensure!(path.is_absolute(), "diagnostic paths must be absolute");
        }
        // Deserialize via the validated pricing parser as well: serde alone does
        // not enforce the domain's discount/rate bounds.
        self.policy.validate()?;
        crate::parse_prices(&serde_json::to_string(&self.prices)?)?;
        crate::parse_prices(&serde_json::to_string(&self.buy_prices)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_or_create_writes_non_spending_defaults_once() {
        let dir = tempfile::tempdir().unwrap();
        let payments = dir.path().join("payments");
        let path = payments.join("payment.json");
        let created = Config::load_or_create(&path, &payments).unwrap();
        assert_eq!(created.data_dir, payments);
        assert!(created.prices.is_empty() && created.buy_prices.is_empty());
        assert_eq!(created.cap_msat, 0);
        assert!(path.is_file(), "config file is written");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&payments).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "payments dir is owner-only");
        }
        // Existing file wins: edits survive a restart.
        std::fs::write(
            &path,
            serde_json::to_vec(&Config {
                cap_msat: 5,
                ..created
            })
            .unwrap(),
        )
        .unwrap();
        let reloaded = Config::load_or_create(&path, &payments).unwrap();
        assert_eq!(reloaded.cap_msat, 5);
    }
    #[test]
    fn load_is_side_effect_free_and_defaults_to_no_spending() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("not-created");
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"data_dir": data})).unwrap(),
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.cap_msat, 0);
        assert!(config.buy_peers.is_empty());
        assert!(!data.exists());
    }
    #[test]
    fn refuses_unknown_fields_and_relative_state() {
        assert!(
            serde_json::from_str::<Config>(r#"{"data_dir":"/tmp/unused","wallet":"legacy"}"#)
                .is_err()
        );
        let config: Config = serde_json::from_str(r#"{"data_dir":"relative"}"#).unwrap();
        assert!(config.validate().is_err());
    }
}
