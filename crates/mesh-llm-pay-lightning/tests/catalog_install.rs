//! The documented install path, through mesh-llm's real plugin-manager.
//!
//! `catalog/plugins.jsonl` holds the catalog lines this repo publishes to
//! the mesh-llm plugin catalog. Users run `mesh-llm plugins install
//! pay-lightning` (and `lexe-wallet`); these tests keep those lines honest.
//!
//! - `catalog_lines_resolve_published_assets` (offline, always runs): every
//!   entry parses, pins a version and a sha256 for each target it lists, and
//!   the asset name the installer would fetch is the one `just package` and
//!   the release workflow produce.
//! - `packaged_archive_installs` (needs `PAY_LIGHTNING_ARCHIVE`): installs the
//!   archive `just package` just built, then runs the installed executable.
//! - `catalog_install_from_release` (needs `PAY_LIGHTNING_CATALOG_INSTALL=1`
//!   and network): serves `catalog/plugins.jsonl` locally and runs the real
//!   `install_plugin("pay-lightning")`, which downloads the pinned GitHub
//!   release asset and verifies its sha256, exactly as `mesh-llm plugins
//!   install pay-lightning` does.
use std::path::{Path, PathBuf};
use std::process::Command;

use mesh_llm_plugin_manager::install::install_plugin_archive;
use mesh_llm_plugin_manager::store::InstalledPluginMetadata;
use mesh_llm_plugin_manager::{
    PluginCatalog, PluginInstallOptions, PluginTarget, PluginVersion, install_plugin,
    select_plugin_asset,
};

const CATALOG: &str = include_str!("../../../catalog/plugins.jsonl");

fn options(root: &Path, catalog_url: String) -> PluginInstallOptions {
    PluginInstallOptions {
        bundled_plugins_dir: None,
        store_root: root.join("store"),
        install_root: root.join("installed"),
        catalog_url,
        target: PluginTarget::current().unwrap(),
    }
}

fn target_for(triple: &str) -> PluginTarget {
    let (arch, os) = match triple {
        "aarch64-apple-darwin" => ("aarch64", "macos"),
        "x86_64-apple-darwin" => ("x86_64", "macos"),
        "x86_64-unknown-linux-gnu" => ("x86_64", "linux"),
        "aarch64-unknown-linux-gnu" => ("aarch64", "linux"),
        "x86_64-pc-windows-msvc" => ("x86_64", "windows"),
        "aarch64-pc-windows-msvc" => ("aarch64", "windows"),
        other => panic!("catalog lists unsupported target {other}"),
    };
    PluginTarget::from_os_arch(os, arch).unwrap()
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
fn catalog_lines_resolve_published_assets() {
    let catalog = PluginCatalog::parse_jsonl(CATALOG).unwrap();
    let entry = catalog
        .find_exact("pay-lightning")
        .expect("pay-lightning entry");
    assert_eq!(
        entry.github_url,
        "https://github.com/Mesh-LLM/ln-payments-plugin"
    );
    let version = env!("CARGO_PKG_VERSION");
    assert_eq!(
        entry.version.as_deref(),
        Some(format!("v{version}").as_str()),
        "catalog pin must match the workspace version"
    );
    for required in [
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
    ] {
        assert!(
            entry.sha256.contains_key(required),
            "no sha256 for {required}"
        );
    }
    assert!(
        catalog.find_exact("lexe-wallet").is_some(),
        "lexe-wallet entry"
    );

    for entry in catalog.entries() {
        let pinned_version = entry.version.clone().unwrap();
        let version = PluginVersion::new(pinned_version.clone()).unwrap();
        for triple in entry.sha256.keys() {
            let pin = entry.pinned_release(triple).unwrap();
            assert_eq!(pin.version, pinned_version);
            let target = target_for(triple);
            let ext = if triple.contains("windows") {
                "zip"
            } else {
                "tar.gz"
            };
            let published = format!("{}-{pinned_version}-{triple}.{ext}", entry.name);
            let asset = select_plugin_asset(
                &entry.name,
                Some(&version),
                &target,
                std::slice::from_ref(&published),
            )
            .unwrap();
            assert_eq!(asset.name, published);
        }
    }
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

/// Serves `body` once per connection on 127.0.0.1; returns its URL.
async fn serve(body: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    format!("http://{addr}/plugins.jsonl")
}

#[tokio::test]
async fn catalog_install_from_release() {
    if std::env::var_os("PAY_LIGHTNING_CATALOG_INSTALL").is_none() {
        eprintln!("PAY_LIGHTNING_CATALOG_INSTALL unset; skipping");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let options = options(root.path(), serve(CATALOG).await);
    let outcome = install_plugin("pay-lightning", &options, &mut |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.metadata.name, "pay-lightning");
    assert_runs(&outcome.metadata);
}
