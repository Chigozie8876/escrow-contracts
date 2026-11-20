#!/usr/bin/env bash
# Generates TS bindings to the specified output directory.
# Usage: OUTPUT_DIR=path/to/output ./scripts/bindings.sh
set -euo pipefail
cd "$(dirname "$0")/.."
./scripts/build.sh
OUTPUT_DIR="${OUTPUT_DIR:-../skillescrow-app/packages/contract-client}"
stellar contract bindings typescript \
  --wasm target/wasm32v1-none/release/bounty_escrow.wasm \
  --output-dir "$OUTPUT_DIR" --overwrite
