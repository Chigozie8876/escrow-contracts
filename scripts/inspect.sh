#!/usr/bin/env bash
#
# Prints the config, the token limits and the totals of the contract at the
# contract id in deployments/<network>.json. Every call is a read-only view
# simulated with --send=no, so nothing is signed or submitted.
#
#   bash scripts/inspect.sh testnet [token-contract-id]
#
# The token defaults to the native XLM asset. Requires the stellar CLI and jq.
# Exits 1 on a missing record, a missing contract_id or a failing call.
set -euo pipefail

network="${1:?usage: inspect.sh <network> [token-contract-id]}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
record="$root/deployments/$network.json"
[ -f "$record" ] || { echo "error: no deployment record at deployments/$network.json" >&2; exit 1; }

id="$(jq -r '.contract_id // empty' "$record")"
[ -n "$id" ] || { echo "error: no contract_id in $record" >&2; exit 1; }

token="${2:-}"
if [ -z "$token" ]; then
  token="$(stellar contract id asset --asset native --network "$network")" \
    || { echo "error: could not derive the native asset id on $network" >&2; exit 1; }
fi

# call <label> <view> [args...]: a read-only simulation. Never signs or submits.
call() {
  local label="$1"; shift
  local out
  if ! out="$(stellar contract invoke --id "$id" --network "$network" --send=no -- "$@" 2>&1)"; then
    echo "error: $label call failed on $network for $id" >&2
    echo "$out" >&2
    exit 1
  fi
  echo "== $label"
  echo "$out"
  echo
}

echo "contract: $id ($network)"
echo "token:    $token"
echo
call config get_config
call "token limits" get_token --token "$token"
call totals get_totals --token "$token"
