#!/usr/bin/env bash
# Review backlog: the Solana relay serves only the site's own pages. A request with no Origin (a
# script, curl) or a foreign one is refused with 403; the site's origin is relayed.
# Usage: bash scripts/check-relay-origin.sh [api url] [a site origin from CORS_ORIGINS]
API=${1:-http://127.0.0.1:4920}
SITE=${2:-http://127.0.0.1:4911}
BODY='{"jsonrpc":"2.0","id":1,"method":"getLatestBlockhash","params":[]}'
none=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/v1/solana-rpc" -H 'content-type: application/json' -d "$BODY")
foreign=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/v1/solana-rpc" -H 'content-type: application/json' -H 'Origin: https://example.com' -d "$BODY")
site=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/v1/solana-rpc" -H 'content-type: application/json' -H "Origin: $SITE" -d "$BODY")
echo "no Origin: $none, foreign Origin: $foreign, the site's Origin: $site"
[ "$none" = 403 ] && [ "$foreign" = 403 ] && [ "$site" != 403 ]
