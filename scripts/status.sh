#!/usr/bin/env bash
# Everything you'd otherwise have to remember, in one screen.
#
#   scripts/status.sh
#
# Reads PAY_TO from the running server's 402 challenge when it can, so the
# numbers always describe the deployment actually in front of you.
set -uo pipefail

KEYPAIR=${KEYPAIR:-payer.json}
CLUSTER=${CLUSTER:-devnet}
MCP=${MCP_URL:-http://127.0.0.1:8080/mcp}
USDC=${USDC_MINT:-4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU}

cd "$(dirname "$0")/.."

bold() { printf '\033[1m%s\033[0m\n' "$1"; }
row()  { printf '  %-22s %s\n' "$1" "$2"; }

usdc_of() { spl-token balance "$USDC" --owner "$1" --url "$CLUSTER" 2>/dev/null || echo "no token account"; }
sol_of()  { solana balance "$1" --url "$CLUSTER" 2>/dev/null || echo "?"; }

# ---------------------------------------------------------------- server
bold "SERVER"
if curl -sf -o /dev/null -m 3 -X POST "$MCP" \
     -H 'Content-Type: application/json' \
     -H 'Accept: application/json, text/event-stream' \
     -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"status","version":"0"}}}'; then
  row "endpoint" "$MCP  UP"
else
  row "endpoint" "$MCP  DOWN"
fi

# ------------------------------------------------------------- challenge
# The live 402 is the authority on price, asset, network and payTo.
CHAL=""
HDRS=$(mktemp); trap 'rm -f "$HDRS"' EXIT
SID=$(curl -s -D - -o /dev/null -m 5 -X POST "$MCP" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"status","version":"0"}}}' \
  2>/dev/null | grep -i '^mcp-session-id:' | tr -d '\r' | awk '{print $2}')
if [ -n "$SID" ]; then
  curl -s -D "$HDRS" -o /dev/null -m 5 -X POST "$MCP" \
    -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
    -H "mcp-session-id: $SID" \
    -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"transcribe_video","arguments":{"url":"probe"}}}' \
    >/dev/null 2>&1
  CHAL=$(grep -i '^payment-required:' "$HDRS" | sed 's/^[Pp]ayment-[Rr]equired: //' | tr -d '\r' | base64 -d 2>/dev/null)
fi

PAY_TO=""; NETWORK=""; ASSET=""; PRICE=""; XVER=""; FEEPAYER=""; TIMEOUT=""
if [ -n "$CHAL" ]; then
  # One space-separated line, read straight into vars — no eval, so nothing
  # in the server's response can be executed as shell.
  CHALF=$(mktemp)
  printf '%s' "$CHAL" > "$CHALF"
  read -r XVER PRICE NETWORK ASSET TIMEOUT PAY_TO FEEPAYER <<EOF
$(python3 - "$CHALF" <<'PY'
import sys, json
d = json.load(open(sys.argv[1]))
a = d["accepts"][0]
print(d.get("x402Version", "?"),
      "%.2f" % (int(a.get("amount", 0)) / 1_000_000),
      a.get("network", "?"),
      a.get("asset", "?"),
      a.get("maxTimeoutSeconds", "?"),
      a.get("payTo", "?"),
      (a.get("extra") or {}).get("feePayer", "-"))
PY
)
EOF
  rm -f "$CHALF"
  echo
  bold "CHALLENGE (live 402)"
  row "x402 version" "${XVER:-?}"
  row "price" "\$${PRICE:-?} USDC"
  row "network" "${NETWORK:-?}"
  row "asset" "${ASSET:-?}"
  row "max timeout" "${TIMEOUT:-?}s   (blockhash expires ~60-90s — see note)"
fi

# ---------------------------------------------------------------- payer
echo
bold "PAYER  (spends)"
if [ -f "$KEYPAIR" ]; then
  P=$(solana address -k "$KEYPAIR")
  row "address" "$P"
  row "USDC" "$(usdc_of "$P")"
  row "SOL" "$(sol_of "$P")   (0 is fine — fees sponsored)"
else
  row "keypair" "MISSING at $KEYPAIR"
fi

# --------------------------------------------------------------- payTo
echo
bold "PAY_TO  (receives)"
if [ -n "$PAY_TO" ]; then
  row "address" "$PAY_TO"
  B=$(usdc_of "$PAY_TO")
  row "USDC" "$B"
  if [ "$B" = "no token account" ]; then
    row "" "^ FIX: spl-token create-account $USDC --url $CLUSTER"
  fi
  row "SOL" "$(sol_of "$PAY_TO")"
else
  row "address" "unknown (server down, or no challenge)"
fi

# --------------------------------------------------------- facilitator
if [ -n "${FEEPAYER:-}" ] && [ "${FEEPAYER}" != "-" ]; then
  echo
  bold "FACILITATOR feePayer  (sponsors network fees)"
  row "address" "$FEEPAYER"
  row "SOL" "$(sol_of "$FEEPAYER")   (dry here = all settlements fail)"
fi

echo
bold "COMMANDS"
row "one paid call" "scripts/pay-once.sh <video>"
row "explorer" "https://explorer.solana.com/address/<addr>?cluster=$CLUSTER"
