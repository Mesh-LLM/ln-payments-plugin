//! The documented install path, through mesh-llm's real plugin-manager.
//!
//! - `packaged_archive_installs` (needs `PAY_LIGHTNING_ARCHIVE`): installs the
//!   archive `just package` just built, then runs the installed executable.
//! - `repo_install_from_release` (needs `PAY_LIGHTNING_RELEASE_INSTALL=1` and
//!   network): runs `mesh-llm plugins install Mesh-LLM/ln-payments-plugin`'s
//!   code path, which picks this platform's published release asset.
use std::path::{Path, PathBuf};
use std::process::Command;

use mesh_llm_plugin_manager::install::install_plugin_archive;
use mesh_llm_plugin_manager::store::InstalledPluginMetadata;
use mesh_llm_plugin_manager::{PluginInstallOptions, PluginTarget, install_plugin};

fn options(root: &Path, catalog_url: String) -> PluginInstallOptions {
    PluginInstallOptions {
        bundled_plugins_dir: None,
        store_root: root.join("store"),
        install_root: root.join("installed"),
        catalog_url,
        target: PluginTarget::current().unwrap(),
    }
}

/// The installed binary answers `--manifest` with this plugin's manifest.
fn assert_runs(metadata: &InstalledPluginMetadata) {
    let exe: PathBuf = metadata.executable_path();
    assert!(
        exe.is_file(),
        "installed executable missing: {}",
        exe.display()
    );
    let output = Command::new(&exe).arg("--manifest").output().unwrap();
    assert!(
        output.status.success(),
        "{} --manifest failed",
        exe.display()
    );
    let manifest: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        manifest["config_schema"]["plugin_name"], "pay-lightning",
        "unexpected manifest: {manifest}"
    );
}

#[test]
fn packaged_archive_installs() {
    let Some(archive) = std::env::var_os("PAY_LIGHTNING_ARCHIVE") else {
        eprintln!("PAY_LIGHTNING_ARCHIVE unset; skipping");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let outcome = install_plugin_archive(
        "pay-lightning",
        env!("CARGO_PKG_VERSION"),
        Path::new(&archive),
        &options(root.path(), "http://unused.invalid".into()),
        &mut |_| {},
    )
    .unwrap();
    assert!(outcome.metadata.enabled);
    assert_runs(&outcome.metadata);
}

#[tokio::test]
async fn repo_install_from_release() {
    if std::env::var_os("PAY_LIGHTNING_RELEASE_INSTALL").is_none() {
        eprintln!("PAY_LIGHTNING_RELEASE_INSTALL unset; skipping");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let options = options(root.path(), "http://unused.invalid".into());
    let outcome = install_plugin("Mesh-LLM/ln-payments-plugin", &options, &mut |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.metadata.name, "pay-lightning");
    assert_runs(&outcome.metadata);
}
