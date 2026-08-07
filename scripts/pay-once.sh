#!/usr/bin/env bash
# One paid tools/call through the proxy, with balances either side.
#
# The JSON-RPC lines are long enough that pasting them into a terminal
# reliably corrupts them, so they are written to a file and redirected in.
#
#   scripts/pay-once.sh                      # bogus URL — should NOT settle
#   scripts/pay-once.sh <video-url>          # real work  — should settle
#
# Requires the upstream server to already be running.
set -euo pipefail

URL=${MCP_URL:-http://127.0.0.1:8080/mcp}
KEYPAIR=${KEYPAIR:-payer.json}
MAX=${MAX_PAYMENTS:-1}
VIDEO=${1:-https://example.com/does-not-exist.mp4}
USDC=${USDC_MINT:-4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU}
CLUSTER=${CLUSTER:-devnet}

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
  echo "RESULT: nothing settled — correct if the tool call failed"
else
  echo "RESULT: settled, $BEFORE -> $AFTER"
fi
