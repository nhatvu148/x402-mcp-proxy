#!/usr/bin/env bash
# Prove the proxy is generic, in one command.
#
# Starts examples/coinflip-server — a server that shares no code with the one
# this proxy was written against, has different tools, and charges a different
# price — then drives it through the proxy unchanged.
#
#   scripts/demo-coinflip.sh
#
# Needs a funded devnet payer at ./payer.json. Costs $0.01 of devnet USDC.
set -uo pipefail
cd "$(dirname "$0")/.."

PORT=${PORT:-8899}
KEYPAIR=${KEYPAIR:-payer.json}
USDC=${USDC_MINT:-4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU}
PAY_TO=${X402_PAY_TO:-7bVktoQRUbdbZxgoeprgDvmjA8kro35FLZFHB2xd7cVU}

[ -f "$KEYPAIR" ] || { echo "no keypair at $KEYPAIR"; exit 1; }
PAYER=$(solana address -k "$KEYPAIR")
bal() { spl-token balance "$USDC" --owner "$PAYER" --url devnet 2>/dev/null || echo "?"; }

echo "building…"
cargo build -p coinflip-server -p x402-mcp-proxy --quiet || exit 1

X402_PAY_TO="$PAY_TO" PORT="$PORT" ./target/debug/coinflip-server >/tmp/coinflip.log 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT

# Wait for it rather than sleeping a guessed interval.
for _ in $(seq 1 40); do
  curl -sf -o /dev/null -m 1 -X POST "http://127.0.0.1:$PORT/mcp" \
    -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' && break
  sleep 0.25
done

BEFORE=$(bal)
echo
echo "payer:       $PAYER"
echo "USDC before: $BEFORE"
echo "server:      http://127.0.0.1:$PORT/mcp  (coinflip — NOT the transcriber)"
echo
echo "--- through the proxy, unchanged ---"

REQ=$(mktemp); trap 'kill $SRV 2>/dev/null; rm -f "$REQ"' EXIT
{
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"demo","version":"0"}}}'
  printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
  printf '%s\n' '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ping","arguments":{}}}'
  printf '%s\n' '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"flip_coin","arguments":{}}}'
} > "$REQ"

./target/debug/x402-mcp-proxy --url "http://127.0.0.1:$PORT/mcp" \
  --keypair "$KEYPAIR" --max-payments 1 < "$REQ" 2>&1 \
  | grep -oE '"name":"coinflip"|"name":"(ping|flip_coin)"|pong \(free\)|heads 🪙|tails 🪙|settled payment [0-9/]+' \
  | sed 's/^/  /'

AFTER=$(bal)
echo
echo "USDC after:  $AFTER"
echo
if [ "$BEFORE" != "$AFTER" ]; then
  echo "✓ The proxy paid this server's price ($BEFORE → $AFTER = \$0.01),"
  echo "  read from ITS 402 challenge — not the \$0.20 the server it was"
  echo "  written against charges. Same binary, one different --url."
else
  echo "✗ nothing settled — check /tmp/coinflip.log"
fi
