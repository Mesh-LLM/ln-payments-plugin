# ln-payments-plugin — developer preview; use caution and small amounts.
set shell := ["bash", "-euo", "pipefail", "-c"]

target := `rustc -vV | sed -n 's/^host: //p'`
exe := if os_family() == "windows" { ".exe" } else { "" }
# mesh-llm plugin-manager expects .zip on Windows targets, .tar.gz elsewhere (target.rs).
ext := if os_family() == "windows" { "zip" } else { "tar.gz" }
version := `sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1`

check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Parallel tests open many ledgers and sockets at once: under macOS's default soft limit of
# 256 open files they fail with "Too many open files (os error 24)", so raise it first.
# Capped at the hard limit, which an unprivileged shell cannot raise.
raise_fd_limit := if os_family() == "unix" { 'want=8192; hard=$(ulimit -Hn); if [ "$hard" != unlimited ] && [ "$hard" -lt "$want" ]; then want=$hard; fi; if [ "$(ulimit -Sn)" != unlimited ] && [ "$(ulimit -Sn)" -lt "$want" ]; then ulimit -Sn "$want"; fi;' } else { "" }

# Offline: unset the release-install switch so an inherited value cannot add network calls.
test:
    {{raise_fd_limit}} env -u PAY_LIGHTNING_RELEASE_INSTALL cargo test --workspace --all-features

build:
    cargo build --release -p mesh-llm-pay-lightning --bin pay-lightning

# Stage and archive the installable plugin: dist/pay-lightning-v<version>-<target>.<ext>
package: build
    rm -rf dist/pay-lightning && mkdir -p dist/pay-lightning
    cp target/release/pay-lightning{{exe}} dist/pay-lightning/
    ./target/release/pay-lightning{{exe}} --manifest > dist/pay-lightning/plugin-manifest.json
    printf 'name = "pay-lightning"\nversion = "{{version}}"\n' > dist/pay-lightning/plugin.toml
    cp crates/mesh-llm-pay-lightning/README.md dist/pay-lightning/README.md
    rm -f "dist/pay-lightning-v{{version}}-{{target}}.{{ext}}"
    if [ "{{ext}}" = zip ]; then (cd dist && 7z a -tzip -bso0 "pay-lightning-v{{version}}-{{target}}.zip" pay-lightning); else tar czf "dist/pay-lightning-v{{version}}-{{target}}.tar.gz" -C dist pay-lightning; fi
    if command -v sha256sum >/dev/null; then sha256sum "dist/pay-lightning-v{{version}}-{{target}}.{{ext}}"; else shasum -a 256 "dist/pay-lightning-v{{version}}-{{target}}.{{ext}}"; fi

# Install the archive `just package` built through mesh-llm's real plugin-manager (offline).
install-check: package
    env -u PAY_LIGHTNING_RELEASE_INSTALL PAY_LIGHTNING_ARCHIVE="$PWD/dist/pay-lightning-v{{version}}-{{target}}.{{ext}}" cargo test -p mesh-llm-pay-lightning --all-features --test plugin_install -- --exact packaged_archive_installs

# Exactly what a PR's CI job runs: the local pre-push check.
ci: check test install-check

# Install the published release by repository name (network, anonymous GitHub API): nightly and after each release, never on PRs.
release-install:
    PAY_LIGHTNING_RELEASE_INSTALL=1 cargo test -p mesh-llm-pay-lightning --all-features --test plugin_install -- --exact repo_install_from_release --nocapture
