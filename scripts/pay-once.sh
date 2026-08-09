#!/usr/bin/env bash
# One paid tools/call through the proxy, with balances either side.
#
# The JSON-RPC lines are long enough that pasting them into a terminal
# reliably corrupts them, so they are written to a file and redirected in.
#
#   scripts/pay-once.sh                      # bogus URL — settles, then refunds a credit
#   scripts/pay-once.sh <video-url>          # real work  — settles, delivers a transcript
#
# Both settle. Payment happens BEFORE execution, because a Solana blockhash
# dies after ~60-90s and settling afterwards silently lost every job longer
# than that. A failed job is therefore already charged, and the server records
# a compensation credit against `wallet:<payer>` instead of withholding
# payment. Check whisgram/api/credits.json (or Postgres) to see it.
#
# Requires the upstream server to already be running.
set -euo pipefail

URL=${MCP_URL:-http://127.0.0.1:8080/mcp}
KEYPAIR=${KEYPAIR:-payer.json}
MAX=${MAX_PAYMENTS:-1}
VIDEO=${1:-https://example.com/does-not-exist.mp4}
CLUSTER=${CLUSTER:-devnet}

# CLUSTER has to drive the proxy too, not just the balance readout. The proxy
# defaults to X402_PROXY_RPC=api.devnet.solana.com, so `CLUSTER=mainnet-beta`
# alone used to leave it fetching devnet blockhashes and devnet accounts while
# paying a mainnet challenge — which fails in a way that looks like a broken
# facilitator rather than a misconfigured client.
# USDC mints are cluster-specific too, so the default follows the cluster.
case "$CLUSTER" in
  mainnet-beta|mainnet)
    RPC_URL=https://api.mainnet-beta.solana.com
    DEFAULT_MINT=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v
    ;;
  devnet)
    RPC_URL=https://api.devnet.solana.com
    DEFAULT_MINT=4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU
    ;;
  *)  # a full RPC URL — caller must name the mint
    RPC_URL=$CLUSTER
    DEFAULT_MINT=${USDC_MINT:?set USDC_MINT when CLUSTER is a raw RPC URL}
    ;;
esac
USDC=${USDC_MINT:-$DEFAULT_MINT}
export X402_PROXY_RPC=${X402_PROXY_RPC:-$RPC_URL}

cd "$(dirname "$0")/.."

[ -f "$KEYPAIR" ] || { echo "no keypair at $KEYPAIR" >&2; exit 1; }
PAYER=$(solana address -k "$KEYPAIR")

balance() {
  spl-token balance "$USDC" --owner "$PAYER" --url "$CLUSTER" 2>/dev/null || echo "?"
}

REQ=$(mktemp)
trap 'rm -f "$REQ"' EXIT

# One JSON-RPC message per line. Built with printf %s so the URL cannot break
# out of the string, and written to a file so no shell wrapping can truncate it.
{
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"pay-once","version":"0"}}}'
  printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  printf '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"transcribe_video","arguments":{"url":"%s"}}}\n' "$VIDEO"
} > "$REQ"

echo "payer:   $PAYER"
echo "cluster: $CLUSTER  (rpc $X402_PROXY_RPC)"
echo "mint:    $USDC"
echo "video:   $VIDEO"
BEFORE=$(balance)
echo "USDC before: $BEFORE"
echo "--- proxy ---"

cargo run --quiet -- \
  --url "$URL" --keypair "$KEYPAIR" --max-payments "$MAX" < "$REQ"

echo "--- done ---"
AFTER=$(balance)
echo "USDC after:  $AFTER"

if [ "$BEFORE" = "$AFTER" ]; then
  echo "RESULT: nothing settled — payment was refused, not just the work"
else
  echo "RESULT: settled, $BEFORE -> $AFTER"
  echo "        (if the tool call FAILED, a compensation credit was recorded —"
  echo "         see credits.json / Postgres. Settling is expected either way.)"
fi
