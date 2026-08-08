# coinflip-server

A tiny x402-gated MCP server that has nothing to do with any other project.

It exists to answer one question: **is `x402-mcp-proxy` generic, or is it
secretly coupled to the server it was written against?**

No shared code — no `rmcp`, no shared crate, no copied module. MCP is spoken by
hand in ~100 lines. Two tools:

| tool | price |
|---|---|
| `ping` | free — so an agent can discover the server without paying |
| `flip_coin` | **$0.01** USDC on solana-devnet |

## Run it

One command, from the repo root — starts the server, drives it through the
proxy, prints the balances either side, cleans up:

```bash
./scripts/demo-coinflip.sh
```

Or by hand, in two terminals:

```bash
# terminal 1
X402_PAY_TO=<your-solana-address> cargo run -p coinflip-server
# → coinflip MCP server on http://127.0.0.1:8899/mcp

# terminal 2 — needs a funded devnet payer at ./payer.json
cargo run --bin x402-mcp-proxy -- --url http://127.0.0.1:8899/mcp \
  --keypair payer.json --max-payments 1
```

Or point Claude Code at it, to see an agent pay a server nobody wrote a client
for:

```bash
claude mcp add -s local coinflip -- \
  "$PWD/target/debug/x402-mcp-proxy" \
  --url http://127.0.0.1:8899/mcp \
  --keypair "$PWD/payer.json" --max-payments 2
```

## What a run proves

```
serverInfo   {"name":"coinflip","version":"0.1.0"}
ping         → "pong (free)"          no charge
flip_coin    → settled payment 1/1    → "tails 🪙"
USDC         18 → 17.99               = exactly $0.01
```

The amount is the point. The proxy paid **$0.01**, this server's price, read from
its own 402 challenge — not the $0.20 the server it was developed against
charges. Price, tools and identity all come from the upstream server; the proxy
only forwards and pays.
