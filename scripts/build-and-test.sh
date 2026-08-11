#!/usr/bin/env bash

set -euo pipefail

echo "Running cargo fmt --all --check"
cargo fmt --all --check

echo "Running cargo clippy --release -- -D warnings"
cargo clippy --release -- -D warnings

echo "Running cargo test --release --all"
cargo test --release --all

ANOVA_WIFI_SSID=ci-dummy-ssid \
ANOVA_WIFI_PASSWORD=ci-dummy-password \
ANOVA_SERVER_URL=http://192.0.2.1:8080 \
cargo build --release

# anova-oven-pico is a standalone workspace (see its Cargo.toml) targeting
# thumbv6m-none-eabi, so it can't be covered by the `--all` commands above.
echo "Checking anova-oven-pico (thumbv6m-none-eabi)"
(
  cd crates/anova-oven-pico
  cargo fmt --check
  export ANOVA_WIFI_SSID=ci-dummy-ssid \
         ANOVA_WIFI_PASSWORD=ci-dummy-password \
         ANOVA_SERVER_URL=http://192.0.2.1:8080
  cargo clippy --release -- -D warnings
  cargo build --release
)
