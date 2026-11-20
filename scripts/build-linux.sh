#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build --release --bin glide
echo "Built Linux binary: target/release/glide"
