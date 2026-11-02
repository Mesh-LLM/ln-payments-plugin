# ln-payments-plugin — WIP, do not use. `just test` is serial: the suite is flaky in parallel.
set shell := ["bash", "-euo", "pipefail", "-c"]

target := `rustc -vV | sed -n 's/^host: //p'`
exe := if os_family() == "windows" { ".exe" } else { "" }
# mesh-llm plugin-manager expects .zip on Windows targets, .tar.gz elsewhere (target.rs).
ext := if os_family() == "windows" { "zip" } else { "tar.gz" }
version := `sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1`

check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings

test:
    cargo test --workspace --all-features -- --test-threads=1

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
