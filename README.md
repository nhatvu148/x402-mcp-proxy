# x402-mcp-proxy

Lets an MCP client that has no wallet use an **x402-gated MCP server**.

```
MCP client ──stdio, free──> x402-mcp-proxy ──HTTP + X-PAYMENT──> paid MCP server
                              (holds wallet)
```

## Why this exists

MCP clients can't pay. `claude mcp add` supports OAuth and static headers —
no wallet, no signer, no 402 handling. And a pinned `-H "X-PAYMENT: ..."` is
not a workaround: the header carries a per-challenge nonce and signature, so a
fixed value is a replay, and any correctly built server refuses it.

So the wallet has to live somewhere else. This proxy is that somewhere.

Note the distinction between the two x402 + MCP patterns:

- **Server pays upstream** — the MCP server holds a wallet and pays third-party
  APIs. Agents see free tools. No proxy needed.
- **Server charges the client** — the MCP server sells its own tools. The
  client must hold a wallet. *This is the case this proxy solves.*

## How it works

It is a transparent forwarder, not an MCP implementation. stdio MCP is
newline-delimited JSON-RPC, so each line is POSTed upstream as-is and the reply
is written back verbatim. There is no method table to keep in sync — new tools
on the upstream server work without touching this code.

Everything x402 (reading the challenge, signing a USDC authorization, retrying)
is handled by [`x402-reqwest`](https://crates.io/crates/x402-reqwest). What this
crate adds is the plumbing MCP needs — session-id propagation, SSE unwrapping —
and a spend cap.

## Install

```bash
cargo install --path .
```

## Usage

Create a payer wallet and fund it with devnet USDC:

```bash
solana-keygen new --derivation-path -o payer.json
solana address -k payer.json
# fund at https://faucet.circle.com → "Solana Devnet"
```

Register with Claude Code:

```bash
claude mcp add my-server -- x402-mcp-proxy \
  --url https://your-server.example.com/mcp \
  --keypair /absolute/path/to/payer.json
```

All flags have environment-variable equivalents (`X402_PROXY_URL`,
`X402_PROXY_KEYPAIR`, `X402_PROXY_RPC`, `X402_PROXY_MAX_PAYMENTS`).

## Spend cap

The proxy holds a funded wallet and an agent can call tools autonomously, so an
agent stuck in a retry loop is a way to drain a wallet unattended.

`--max-payments` (default `10`) bounds this. Once the cap is reached, paid
calls are refused **at the proxy** with a JSON-RPC error; discovery calls stay
free. `--max-payments 0` refuses all paid calls, which is a useful way to see
which calls actually cost money.

The counter tracks *settlements*, not requests — so free calls, and calls whose
work failed upstream (which a correct server declines to settle), don't consume
budget.

The cap resets when the proxy restarts. Treat the payer as a hot wallet: fund
it with what you're willing to lose, not with your balance.

## Wallet backup warning

`solana-keygen recover` silently returns the **wrong address** if you use the
wrong derivation. There is no error — it looks exactly like a lost wallet:

| Created with | Recover with |
|---|---|
| `solana-keygen new` (no flags) | `ASK` |
| `solana-keygen new --derivation-path` | `'prompt://?key=0/0'` |

Use `--derivation-path` (as shown above) — that path is `m/44'/501'/0'/0'`,
which is also what Phantom, Solflare, and Ledger use, so the seed phrase stays
portable. **Verify recovery reproduces the address before funding the wallet.**

## Status

Early. The forwarding, session handling, SSE unwrapping and spend cap are
implemented and unit-tested. Not yet exercised against a live facilitator with
a funded wallet — see the upstream server's handoff notes for the end-to-end
settlement test.

## License

MIT
