#!/usr/bin/env bash
# Runs solana-test-validator on the Acrux chain ports, in the foreground.
# usage: validator.sh <state-dir> <slots-per-epoch> [extra solana-test-validator args]
# Writes the validator's PID to <state-dir>/validator.pid and logs to <state-dir>/validator.log.
set -euo pipefail
export PATH="$HOME/.local/share/solana/install/active_release/bin:/usr/local/bin:/usr/bin:/bin"

state="$1"
slots_per_epoch="$2"
shift 2
chain="$(cd "$(dirname "$0")/.." && pwd)"
authority="$chain/keys/dev/local-authority.json"
hub_registry="$(solana-keygen pubkey "$chain/target/deploy/hub_registry-keypair.json" 2>/dev/null \
  || sed -n 's/^hub_registry = "\(.*\)"/\1/p' "$chain/Anchor.toml")"

mkdir -p "$state"
echo $$ > "$state/validator.pid"
export RUST_LOG=warn
exec solana-test-validator \
  --ledger "$state/ledger" --reset --quiet \
  --bind-address 127.0.0.1 \
  --rpc-port 4990 --faucet-port 4992 --gossip-port 4993 --dynamic-port-range 4994-5015 \
  --slots-per-epoch "$slots_per_epoch" \
  --mint "$(solana-keygen pubkey "$authority")" \
  --upgradeable-program "$hub_registry" "$chain/target/deploy/hub_registry.so" "$authority" \
  "$@" > "$state/validator.log" 2>&1
