#!/usr/bin/env bash
# Road build 1, step 1: the stock solana CLI against Acrux, with only the URL changed.
# A fresh key gets test ACRUX from the faucet, then solana balance, solana transfer and
# solana confirm -v. Usage (Git Bash or Linux, solana CLI on PATH):
#   bash step1.sh https://rpc.acrux.computer https://api.acrux.computer/v1/faucet
#   bash step1.sh http://127.0.0.1:4930 http://127.0.0.1:4989/internal/faucet
# A third argument, a keypair file already holding ACRUX, is used instead of the faucet:
#   bash step1.sh https://rpc.acrux.computer - funded.json
U=${1:-https://rpc.acrux.computer}
FAUCET=${2:-https://api.acrux.computer/v1/faucet}
export TZ=UTC
DIR=$(mktemp -d)
if [ -n "$3" ]; then cp "$3" "$DIR/payer.json"; else solana-keygen new --no-bip39-passphrase -s -o "$DIR/payer.json" --force >/dev/null; fi
solana-keygen new --no-bip39-passphrase -s -o "$DIR/to.json" --force >/dev/null
P=$(solana-keygen pubkey "$DIR/payer.json"); TO=$(solana-keygen pubkey "$DIR/to.json")
case "$FAUCET" in
  *internal*) BODY="{\"address\":\"$P\",\"amountLamports\":\"2000000000\"}" ;;
  *) BODY="{\"address\":\"$P\"}" ;;
esac
[ -n "$3" ] || curl -s -X POST "$FAUCET" -H 'content-type: application/json' -d "$BODY" >/dev/null
for i in $(seq 30); do [ "$(solana balance -u "$U" -k "$DIR/payer.json")" != "0 SOL" ] && break; sleep 1; done
echo "\$ solana balance"; solana balance -u "$U" -k "$DIR/payer.json"
echo "\$ solana transfer $TO 0.001"; OUT=$(solana transfer -u "$U" -k "$DIR/payer.json" "$TO" 0.001 --allow-unfunded-recipient 2>&1); echo "$OUT"
SIG=$(echo "$OUT" | grep -o 'Signature: [1-9A-HJ-NP-Za-km-z]*' | cut -d' ' -f2)
echo "\$ solana balance $TO"; solana balance -u "$U" "$TO"
sleep 2
echo "\$ solana confirm -v"; solana confirm -u "$U" -k "$DIR/payer.json" -v "$SIG"
echo "/tx: $SIG"
rm -rf "$DIR"
