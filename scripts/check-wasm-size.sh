#!/usr/bin/env bash
# Fails if the built contract Wasm exceeds the configured size ceiling.
# The ceiling is intentionally above the current size so it catches growth,
# not ordinary compiler noise. Override with MAX_WASM_BYTES for a one-off run.
set -euo pipefail
cd "$(dirname "$0")/.."

WASM=${WASM:-target/wasm32v1-none/release/bounty_escrow.wasm}
MAX_WASM_BYTES=${MAX_WASM_BYTES:-40960} # 40 KiB

if [ ! -f "$WASM" ]; then
  echo "error: $WASM not found; run ./scripts/build.sh first" >&2
  exit 1
fi

size=$(wc -c < "$WASM" | tr -d ' ')
if command -v sha256sum >/dev/null 2>&1; then
  hash=$(sha256sum "$WASM" | cut -d' ' -f1)
elif command -v shasum >/dev/null 2>&1; then
  hash=$(shasum -a 256 "$WASM" | cut -d' ' -f1)
else
  echo "error: neither sha256sum nor shasum is available; install coreutils or perl" >&2
  exit 1
fi

headroom=$(( MAX_WASM_BYTES - size ))
echo "wasm size: $size bytes (ceiling: $MAX_WASM_BYTES bytes, headroom: $headroom bytes)"
echo "wasm sha256: $hash"

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo '### Wasm size'
    echo
    echo '| Metric | Value |'
    echo '|---|---:|'
    echo "| Size | $size bytes |"
    echo "| Ceiling | $MAX_WASM_BYTES bytes |"
    echo "| Headroom | $headroom bytes |"
    echo "| SHA-256 | \`$hash\` |"
  } >> "$GITHUB_STEP_SUMMARY"
fi

if (( size > MAX_WASM_BYTES )); then
  echo "error: $WASM is $size bytes, above the $MAX_WASM_BYTES-byte ceiling" >&2
  exit 1
fi
