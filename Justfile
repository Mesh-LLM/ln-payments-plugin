# ln-payments-plugin — WIP, do not use.
set shell := ["bash", "-euo", "pipefail", "-c"]

target := `rustc -vV | sed -n 's/^host: //p'`
version := `sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1`

check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace -- --test-threads=1

build:
    cargo build --release -p mesh-llm-pay-lightning --bin pay-lightning

# Stage and archive the installable plugin: dist/pay-lightning-v<version>-<target>.tar.gz
package: build
    rm -rf dist/pay-lightning && mkdir -p dist/pay-lightning
    cp target/release/pay-lightning dist/pay-lightning/
    ./target/release/pay-lightning --manifest > dist/pay-lightning/plugin-manifest.json
    printf 'name = "pay-lightning"\nversion = "{{version}}"\n' > dist/pay-lightning/plugin.toml
    cp crates/mesh-llm-pay-lightning/README.md dist/pay-lightning/README.md
    tar czf "dist/pay-lightning-v{{version}}-{{target}}.tar.gz" -C dist pay-lightning
    shasum -a 256 "dist/pay-lightning-v{{version}}-{{target}}.tar.gz"
