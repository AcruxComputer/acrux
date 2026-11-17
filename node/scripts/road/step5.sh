#!/usr/bin/env bash
# Road build 1, step 5: an Anchor workspace (backend/chain/examples/counter) deployed to Acrux and
# its test suite run, with only the provider's cluster changed. Each run takes a fresh program id
# (`anchor keys sync`), builds for SBF v0 (what Solana mainnet runs), deploys with `anchor deploy`
# (passing --use-rpc through to `solana program deploy`) and runs the suite against the deployed
# program. Usage, from the project's backend/ folder (Git Bash or Linux; Anchor 0.31, the solana
# CLI and yarn on PATH):
#   bash node/scripts/road/step5.sh https://rpc.acrux.computer https://api.acrux.computer/v1/faucet
#   bash node/scripts/road/step5.sh https://rpc.acrux.computer - funded.json
# The deploy holds about 1.3 ACRUX of rent; the faucet is asked twice when no keypair is given.
U=${1:-https://rpc.acrux.computer}
FAUCET=${2:-https://api.acrux.computer/v1/faucet}
export TZ=UTC
DIR=$(mktemp -d)
if [ -n "$3" ]; then cp "$3" "$DIR/wallet.json"; else solana-keygen new --no-bip39-passphrase -s -o "$DIR/wallet.json" --force >/dev/null; fi
P=$(solana-keygen pubkey "$DIR/wallet.json")
if [ -z "$3" ]; then
  for amount in 2000000000 2000000001; do
    case "$FAUCET" in
      *internal*) BODY="{\"address\":\"$P\",\"amountLamports\":\"$amount\"}" ;;
      *) BODY="{\"address\":\"$P\"}" ;;
    esac
    curl -s -X POST "$FAUCET" -H 'content-type: application/json' -d "$BODY" >/dev/null
    sleep 2
  done
fi
for i in $(seq 30); do [ "$(solana balance -u "$U" "$P")" != "0 SOL" ] && break; sleep 1; done
cd chain/examples/counter
rm -f target/deploy/counter-keypair.json
solana-keygen new --no-bip39-passphrase -s -o target/deploy/counter-keypair.json --force >/dev/null
anchor keys sync >/dev/null
echo "\$ anchor build"
anchor build --no-idl -- --arch v0 2>&1 | grep -E "Finished|^error" | head -2
anchor idl build -o target/idl/counter.json -t target/types/counter.ts 2>&1 | grep -E "^error" | head -2
echo "\$ anchor deploy --provider.cluster $U -- --use-rpc --buffer <fresh>"
# A fresh buffer each run: the newer solana CLI otherwise derives one that an earlier run may hold.
solana-keygen new --no-bip39-passphrase -s -o "$DIR/buffer.json" --force >/dev/null
anchor deploy --provider.cluster "$U" --provider.wallet "$DIR/wallet.json" -- --use-rpc --buffer "$DIR/buffer.json" 2>&1 | grep -E "Deploying|Program Id|Signature|Deploy success|Error"
echo "\$ anchor test --skip-local-validator --skip-deploy --provider.cluster $U"
case "$(uname -s)" in
  # Anchor on Windows hands its test script to WSL's bash; run that same script here, as it would.
  MINGW*|MSYS*) ANCHOR_PROVIDER_URL="$U" ANCHOR_WALLET="$(cygpath -w "$DIR/wallet.json")" yarn run ts-mocha -p ./tsconfig.json -t 1000000 "tests/**/*.ts" 2>&1 | grep -v "MODULE_TYPELESS\|Reparsing\|eliminate this warning\|trace-warnings" ;;
  *) anchor test --skip-local-validator --skip-deploy --skip-build --provider.cluster "$U" --provider.wallet "$DIR/wallet.json" ;;
esac
rm -rf "$DIR"
