# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.5] - 2026-08-12

### Fixed

- **A stalled upstream or RPC call now fails instead of hanging forever.**
  `reqwest` has no default request timeout, so a connection that stopped
  responding waited indefinitely and the MCP client saw a tool that simply never
  answered. A `transcribe_video` hung for 30 minutes before the harness killed
  it, having settled nothing and logged nothing.

  The upstream timeout is deliberately generous rather than snappy: this proxy
  fronts long synchronous work, and the response arrives in one piece at the
  end, so no bytes flow in between and anything aggressive would kill healthy
  calls. 15 minutes is a backstop against a wedged connection, not a policy on
  how long work may take. `connect_timeout` is separate and short, because a
  connection that cannot be established is a different failure.

  Solana RPC calls get their own, much shorter bound — they are small and fast,
  and a public endpoint that has begun rate-limiting should fail quickly rather
  than stall a payment behind it. Startup now also names a public RPC as
  rate-limited, since that is the most likely thing here to stall and a slow
  payment otherwise reads as a broken one.

  Configurable via `--timeout-secs` / `X402_PROXY_TIMEOUT_SECS` and
  `--rpc-timeout-secs` / `X402_PROXY_RPC_TIMEOUT_SECS`.

## [0.1.4] - 2026-08-11

### Fixed

- **One slow call no longer blocks every other one.** Each message was forwarded
  and awaited before the next line of stdin was read, so a transcription running
  for minutes stalled everything queued behind it — including free discovery
  calls. From the client's side that is indistinguishable from a dead server.

  Each message now forwards on its own task, with the writer behind a mutex so
  two replies cannot interleave mid-line. JSON-RPC ids mean replies may return
  out of order; the client matches them up.

  `initialize` stays serialized: it mints the session id every later request
  must carry, so anything racing it goes out session-less and is refused.

  The task set is drained rather than dropped — stdin closing does not mean the
  work is done, and aborting there would discard a transcription the caller has
  already paid for.

## [0.1.3] - 2026-08-10

### Fixed

- **Upstream POST errors name the configured RPC**, which turns a mainnet mint
  failing to unpack on a devnet RPC from a mystery into a one-line diagnosis.

## [0.1.2] - 2026-08-09

Initial published release.
