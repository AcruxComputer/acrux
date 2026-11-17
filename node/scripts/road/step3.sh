#!/usr/bin/env bash
# Road build 1, step 3: a program deployed to Acrux with the stock `solana program deploy` and
# called. A fresh key gets test ACRUX from the faucet, deploys backend/chain/examples/hello under
# a fresh program id through the upgradeable loader, shows it, and calls it with web3.js.
# Usage (Git Bash or Linux, solana CLI on PATH, run from the project's backend/ folder):
#   bash node/scripts/road/step3.sh https://rpc.acrux.computer https://api.acrux.computer/v1/faucet
#   bash node/scripts/road/step3.sh http://127.0.0.1:4930 http://127.0.0.1:4989/internal/faucet
U=${1:-https://rpc.acrux.computer}
FAUCET=${2:-https://api.acrux.computer/v1/faucet}
SO=chain/examples/hello/target/deploy/hello.so
export TZ=UTC
DIR=$(mktemp -d)
solana-keygen new --no-bip39-passphrase -s -o "$DIR/deployer.json" --force >/dev/null
solana-keygen new --no-bip39-passphrase -s -o "$DIR/program.json" --force >/dev/null
P=$(solana-keygen pubkey "$DIR/deployer.json"); PROGRAM=$(solana-keygen pubkey "$DIR/program.json")
case "$FAUCET" in
  *internal*) BODY="{\"address\":\"$P\",\"amountLamports\":\"2000000000\"}" ;;
  *) BODY="{\"address\":\"$P\"}" ;;
esac
curl -s -X POST "$FAUCET" -H 'content-type: application/json' -d "$BODY" >/dev/null
for i in $(seq 30); do [ "$(solana balance -u "$U" -k "$DIR/deployer.json")" != "0 SOL" ] && break; sleep 1; done
echo "\$ solana program deploy --use-rpc hello.so"
solana program deploy -u "$U" -k "$DIR/deployer.json" --use-rpc --program-id "$DIR/program.json" "$SO"
echo "\$ solana program show $PROGRAM"
solana program show -u "$U" -k "$DIR/deployer.json" "$PROGRAM"
sleep 1
(cd sdk && node test/step3-call.mjs "$PROGRAM" "$DIR/deployer.json" "$U")
rm -rf "$DIR"
