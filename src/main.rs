//! Lets an MCP client that has no wallet talk to an x402-gated MCP server.
//!
//! Claude Code (and every other MCP client today) speaks MCP but cannot pay:
//! `claude mcp add` offers OAuth and static headers, nothing else. A static
//! `X-PAYMENT` header is not a workaround either — the header carries a
//! per-challenge nonce and signature, so a pinned value is a replay and a
//! correctly built server refuses it.
//!
//! This proxy sits in the middle and holds the wallet:
//!
//! ```text
//! MCP client ──stdio, free──> proxy (wallet) ──HTTP + X-PAYMENT──> paid server
//! ```
//!
//! It is deliberately a *transparent forwarder* rather than an MCP
//! implementation: stdio MCP is newline-delimited JSON-RPC, so each line is
//! POSTed upstream as-is and the reply is written back verbatim. No method
//! table to keep in sync with the upstream server, and new tools work without
//! touching this code.
//!
//! Everything x402 — reading the 402 challenge, signing a USDC authorization,
//! retrying — is handled by `x402-reqwest`. The only judgement here is the
//! spend cap (see [`Budget`]).
//!
//! # stdout is the protocol
//!
//! Only JSON-RPC replies may go to stdout. All logging goes to stderr; a stray
//! `println!` corrupts the stream and the client drops the connection.

use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest_middleware::ClientWithMiddleware;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_keypair::{Keypair, read_keypair_file};
use solana_signer::Signer;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use x402_chain_solana::V2SolanaExactClient;
use x402_reqwest::{ReqwestWithPayments, ReqwestWithPaymentsBuild, X402Client};

/// Header the MCP streamable-HTTP transport uses to bind requests to a session.
const MCP_SESSION_ID: &str = "mcp-session-id";

/// Set by x402 on a response that actually settled. Used to count real spends.
///
/// The name has no `x-` prefix — see `x402-axum/src/paygate.rs:559`, which
/// inserts `Payment-Response`. Guessing `x-payment-response` (by analogy with
/// the *request* header `X-PAYMENT`) made the spend cap silently inert: every
/// settlement went uncounted, so `--max-payments` never stopped anything.
/// Lowercase because `HeaderMap` normalises names on lookup.
const PAYMENT_RESPONSE: &str = "payment-response";

/// Carries the base64 x402 challenge on a 402. The body of a 402 is empty, so
/// this header is the only place the reason for refusal appears.
const PAYMENT_REQUIRED: &str = "payment-required";

#[derive(Parser, Debug)]
#[command(
    name = "x402-mcp-proxy",
    // Packaging (Homebrew, distro formulae) checks `--version` to prove the
    // installed binary actually runs; without this clap rejects the flag.
    version,
    about = "Pay-per-call bridge between a walletless MCP client and an x402-gated MCP server"
)]
struct Args {
    /// Upstream MCP endpoint, e.g. https://example.com/mcp
    #[arg(long, env = "X402_PROXY_URL")]
    url: String,

    /// Solana keypair JSON that funds payments.
    ///
    /// Create with `solana-keygen new --derivation-path -o payer.json`. The
    /// `--derivation-path` matters: without it the seed phrase only recovers
    /// via the `ASK` keyword, not `prompt://`, which reads as a lost wallet.
    #[arg(long, env = "X402_PROXY_KEYPAIR")]
    keypair: String,

    /// Solana RPC endpoint. Needed to build and simulate the payment.
    #[arg(
        long,
        env = "X402_PROXY_RPC",
        default_value = "https://api.devnet.solana.com"
    )]
    rpc: String,

    /// Maximum number of payments to settle before refusing further paid calls.
    ///
    /// An agent in a retry loop can otherwise drain the wallet unattended. 0
    /// disables paying entirely — useful to confirm which calls are free.
    #[arg(long, env = "X402_PROXY_MAX_PAYMENTS", default_value_t = 10)]
    max_payments: usize,

    /// Give up on an upstream call after this many seconds.
    ///
    /// `reqwest` has NO default request timeout, so without this a stalled
    /// connection waits forever and the MCP client sees a tool that never
    /// answers. That is not hypothetical: on 2026-08-11 a `transcribe_video`
    /// hung for 30 minutes and was killed by the harness, having settled
    /// nothing and logged nothing.
    ///
    /// The default is deliberately generous rather than snappy. This proxy
    /// fronts long synchronous work — a transcription runs for minutes and the
    /// response arrives in one piece at the end, so no bytes flow in between
    /// and anything aggressive would kill healthy calls. 15 minutes sits above
    /// the upstream's own ceiling, making this a backstop against a wedged
    /// connection, not a policy on how long work may take.
    #[arg(long, env = "X402_PROXY_TIMEOUT_SECS", default_value_t = 900)]
    timeout_secs: u64,

    /// Give up on a Solana RPC call after this many seconds.
    ///
    /// Separate from `timeout_secs` because these are different animals: RPC
    /// calls are small and fast, and a public endpoint that has started
    /// rate-limiting should fail quickly so the caller learns why, rather than
    /// stalling a payment behind it.
    #[arg(long, env = "X402_PROXY_RPC_TIMEOUT_SECS", default_value_t = 30)]
    rpc_timeout_secs: u64,
}

/// Counts settled payments and stops the proxy once the cap is reached.
///
/// Counting *settlements* rather than requests is deliberate: free calls
/// (`initialize`, `tools/list`) and calls whose work failed — which the server
/// declines to settle — must not consume budget.
struct Budget {
    spent: AtomicUsize,
    max: usize,
}

impl Budget {
    fn new(max: usize) -> Self {
        Self {
            spent: AtomicUsize::new(0),
            max,
        }
    }

    fn exhausted(&self) -> bool {
        self.spent.load(Ordering::Relaxed) >= self.max
    }

    /// Returns the new total.
    fn record(&self) -> usize {
        self.spent.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn spent(&self) -> usize {
        self.spent.load(Ordering::Relaxed)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if std::io::stdin().is_terminal() {
        eprintln!(
            "x402-mcp-proxy speaks MCP over stdio and is meant to be launched by an\n\
             MCP client, not run by hand. Register it with, e.g.:\n\n  \
             claude mcp add my-server -- x402-mcp-proxy --url {} --keypair {}\n",
            args.url, args.keypair
        );
    }

    let keypair: Keypair = read_keypair_file(&args.keypair)
        .map_err(|e| anyhow::anyhow!("read keypair {}: {e}", args.keypair))?;
    let payer = keypair.pubkey().to_string();
    let signer = Arc::new(keypair);
    let rpc = Arc::new(RpcClient::new_with_timeout(
        args.rpc.clone(),
        Duration::from_secs(args.rpc_timeout_secs),
    ));
    // The public endpoints are rate-limited and are the most likely thing here
    // to stall under load. Say so once at startup rather than leaving a slow
    // payment looking like a broken one.
    if args.rpc.contains("api.mainnet-beta.solana.com")
        || args.rpc.contains("api.devnet.solana.com")
    {
        eprintln!(
            "  note:  {} is a public rate-limited RPC; payments may be slow or \
             fail under load. Set X402_PROXY_RPC to a dedicated endpoint.",
            args.rpc
        );
    }

    // x402 v2 — identifies networks by CAIP-2 chain id rather than v1's
    // `"solana-devnet"` name. Server, client and facilitator must all agree on
    // the version, so this moves in lockstep with the upstream server's
    // `V2SolanaExact::price_tag` and the facilitator's `v2-solana-exact` scheme.
    let x402 = X402Client::new().register(V2SolanaExactClient::new(signer, rpc));
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(args.timeout_secs))
        // Short and separate: a connection that cannot be established is a
        // different failure from work that is taking a while, and should not
        // wait out the generous request timeout to say so.
        .connect_timeout(Duration::from_secs(10))
        .build()
        .context("build HTTP client")?
        .with_payments(x402)
        .build();

    let budget = Budget::new(args.max_payments);

    eprintln!(
        "x402-mcp-proxy → {}\n  payer: {}\n  rpc:   {}\n  cap:   {} payments",
        args.url, payer, args.rpc, args.max_payments
    );

    pump(http, Arc::new(args), Arc::new(budget)).await
}

/// Reads newline-delimited JSON-RPC from stdin, forwards each message, writes
/// replies to stdout.
///
/// Each message is forwarded on its own task. The first version awaited every
/// forward before reading the next line, which meant one slow call blocked the
/// whole pipe: a transcription running for minutes would stall the free
/// `get_latest_transcript` queued behind it, and both would look like the
/// server had hung. It made three unrelated production failures present
/// identically, and hid a fourth entirely.
///
/// JSON-RPC carries an `id` on every request, so replies may return in any
/// order — the client matches them up. Notifications have no id and no reply.
async fn pump(http: ClientWithMiddleware, args: Arc<Args>, budget: Arc<Budget>) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    // One writer, shared: two tasks writing at once would interleave bytes
    // mid-line and produce JSON no client can parse.
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
    // Set by whichever request establishes the session (initialize) and read by
    // every request after. MCP clients wait for the initialize reply before
    // sending anything else, so this is written once before it is read.
    let session: Arc<Mutex<Option<HeaderValue>>> = Arc::new(Mutex::new(None));
    let mut tasks = tokio::task::JoinSet::new();

    while let Some(line) = lines.next_line().await.context("read stdin")? {
        if line.trim().is_empty() {
            continue;
        }

        // Refuse locally once the cap is hit, so a runaway agent stops at the
        // proxy instead of at the wallet.
        if budget.exhausted() && is_paid_call(&line) {
            if let Some(reply) = budget_error(&line, &budget) {
                write_line(&stdout, &reply).await?;
            }
            continue;
        }

        // `initialize` establishes the session every later request needs, so it
        // is the one message that must complete before the next is sent.
        // Everything else runs concurrently.
        if is_initialize(&line) {
            deliver(&http, &args, &line, &session, &budget, &stdout).await;
            continue;
        }

        let http = http.clone();
        let args = args.clone();
        let budget = budget.clone();
        let session = session.clone();
        let stdout = stdout.clone();
        tasks.spawn(async move {
            deliver(&http, &args, &line, &session, &budget, &stdout).await;
        });
    }

    // stdin closing does not mean the work is done. Dropping the JoinSet here
    // would abort a transcription the client already paid for.
    while tasks.join_next().await.is_some() {}

    eprintln!("stdin closed; {} payment(s) settled", budget.spent());
    Ok(())
}

/// Forward one message and write whatever comes back.
///
/// Shared by the serialized `initialize` path and the concurrent one, so both
/// handle replies, notifications and transport errors identically.
async fn deliver(
    http: &ClientWithMiddleware,
    args: &Args,
    line: &str,
    session: &Mutex<Option<HeaderValue>>,
    budget: &Budget,
    stdout: &Mutex<tokio::io::Stdout>,
) {
    match forward(http, args, line, session, budget).await {
        Ok(Some(reply)) => {
            let _ = write_line(stdout, &reply).await;
        }
        // Notifications get no reply; upstream returned 202 with no body.
        Ok(None) => {}
        Err(e) => {
            eprintln!("forward failed: {e:#}");
            if let Some(reply) = transport_error(line, &e) {
                let _ = write_line(stdout, &reply).await;
            }
        }
    }
}

async fn write_line(stdout: &Mutex<tokio::io::Stdout>, s: &str) -> Result<()> {
    // Held across all three writes so a concurrent reply cannot land mid-line.
    let mut out = stdout.lock().await;
    out.write_all(s.as_bytes()).await?;
    out.write_all(b"\n").await?;
    out.flush().await?;
    Ok(())
}

/// POSTs one JSON-RPC message upstream and returns the reply body, if any.
async fn forward(
    http: &ClientWithMiddleware,
    args: &Args,
    line: &str,
    session: &Mutex<Option<HeaderValue>>,
    budget: &Budget,
) -> Result<Option<String>> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    // The streamable-HTTP transport may answer either way, so accept both.
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    if let Some(id) = session.lock().await.as_ref() {
        headers.insert(HeaderName::from_static(MCP_SESSION_ID), id.clone());
    }

    // Name the RPC in the error. Payment signing reads the mint account from
    // whichever cluster this points at, so pointing it at the wrong one fails
    // deep inside the x402 layer with something like "failed to unpack mint
    // <mainnet USDC>: unknown owner" — because that address also exists on
    // devnet, as an ordinary wallet rather than a mint. Without the RPC in the
    // message that error reads like a broken server instead of a misconfigured
    // client.
    let response = http
        .post(&args.url)
        .headers(headers)
        .body(line.to_owned())
        .send()
        .await
        .with_context(|| format!("POST upstream (proxy rpc: {})", args.rpc))?;

    // The server issues a session id on initialize; every later request must
    // carry it or it is treated as a new, unknown session.
    if let Some(id) = response.headers().get(MCP_SESSION_ID) {
        *session.lock().await = Some(id.clone());
    }

    if response.headers().contains_key(PAYMENT_RESPONSE) {
        let total = budget.record();
        eprintln!("settled payment {}/{}", total, args.max_payments);
    }

    let status = response.status();
    // An unsatisfied 402 carries its detail in this header, never in the body,
    // so it has to be read before the response is consumed.
    let challenge = response
        .headers()
        .get(PAYMENT_REQUIRED)
        .and_then(|v| v.to_str().ok())
        .map(decode_challenge);

    let body = response.text().await.context("read upstream body")?;

    if body.trim().is_empty() {
        if let Some(detail) = challenge {
            // Reaching here means x402 could not satisfy the challenge —
            // typically an unfunded payer or an expired quote. Reporting
            // "empty body" would bury the actual reason.
            anyhow::bail!("payment required and not completed: {detail}");
        }
        if !status.is_success() {
            anyhow::bail!("upstream returned {status} with an empty body");
        }
        return Ok(None);
    }

    Ok(Some(unwrap_sse(&body)))
}

/// Renders a base64 x402 challenge as something a human can act on.
///
/// Falls back to the raw header when it can't be decoded — a truncated blob is
/// still better than discarding the only diagnostic the server sent.
fn decode_challenge(raw: &str) -> String {
    use base64::Engine;
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(raw) else {
        return format!("<undecodable challenge: {}>", truncate(raw, 80));
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return "<challenge was not valid UTF-8>".to_owned();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return truncate(&text, 300);
    };

    // Surface the fields that identify *why* a payment didn't go through.
    let err = json.get("error").and_then(|v| v.as_str()).unwrap_or("-");
    let first = json
        .get("accepts")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first());
    match first {
        Some(a) => format!(
            "{err} (want {} of {} on {}, to {})",
            a.get("amount").and_then(|v| v.as_str()).unwrap_or("?"),
            a.get("asset").and_then(|v| v.as_str()).unwrap_or("?"),
            a.get("network").and_then(|v| v.as_str()).unwrap_or("?"),
            a.get("payTo").and_then(|v| v.as_str()).unwrap_or("?"),
        ),
        None => err.to_owned(),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_owned()
    } else {
        format!("{}…", &s[..n])
    }
}

/// Extracts JSON from an SSE frame, leaving plain JSON untouched.
///
/// The streamable-HTTP transport may reply as `text/event-stream`, where the
/// payload sits in `data:` lines. Forwarding the frame verbatim would hand the
/// stdio client something it cannot parse.
fn unwrap_sse(body: &str) -> String {
    let data: Vec<&str> = body
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|l| l.trim())
        .collect();

    if data.is_empty() {
        body.trim().to_owned()
    } else {
        data.join("")
    }
}

/// `initialize` is the one message that cannot be concurrent.
///
/// It is what mints the session id every later request must carry, so anything
/// sent alongside it would go out session-less and be refused. Clients happen
/// to wait for the reply before sending more, but a proxy must not depend on
/// the client being polite — feeding a pipelined script through it did exactly
/// this and only `initialize` came back.
fn is_initialize(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("method")?.as_str().map(|m| m == "initialize"))
        .unwrap_or(false)
}

/// True when the message is a `tools/call`, i.e. the only thing that can cost
/// money. Discovery methods are free by design and must never be blocked.
fn is_paid_call(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("method")?.as_str().map(|m| m == "tools/call"))
        .unwrap_or(false)
}

fn request_id(line: &str) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    // Notifications carry no id and expect no reply.
    v.get("id").cloned()
}

fn error_reply(id: serde_json::Value, code: i64, message: String) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
    .to_string()
}

fn budget_error(line: &str, budget: &Budget) -> Option<String> {
    let id = request_id(line)?;
    Some(error_reply(
        id,
        -32000,
        format!(
            "x402-mcp-proxy spend cap reached ({}/{} payments settled). \
             Restart with a higher --max-payments to continue.",
            budget.spent(),
            budget.max
        ),
    ))
}

fn transport_error(line: &str, e: &anyhow::Error) -> Option<String> {
    let id = request_id(line)?;
    Some(error_reply(id, -32603, format!("proxy: {e:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_json_passes_through_unchanged() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        assert_eq!(unwrap_sse(body), body);
    }

    #[test]
    fn sse_frames_are_unwrapped_to_their_payload() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1}\n\n";
        assert_eq!(unwrap_sse(body), r#"{"jsonrpc":"2.0","id":1}"#);
    }

    #[test]
    fn multi_line_sse_payloads_are_rejoined() {
        let body = "data: {\"a\":1,\n data: \"b\":2}\n\n";
        assert!(unwrap_sse(body).contains("\"a\":1"));
    }

    #[test]
    fn only_tools_call_is_treated_as_paid() {
        assert!(is_paid_call(r#"{"method":"tools/call","id":1}"#));
        assert!(!is_paid_call(r#"{"method":"tools/list","id":1}"#));
        assert!(!is_paid_call(r#"{"method":"initialize","id":1}"#));
        assert!(!is_paid_call("not json"));
    }

    #[test]
    fn notifications_get_no_error_reply() {
        // No id → nothing to correlate a reply to, so stay silent.
        let budget = Budget::new(0);
        assert!(budget_error(r#"{"method":"tools/call"}"#, &budget).is_none());
        assert!(budget_error(r#"{"method":"tools/call","id":7}"#, &budget).is_some());
    }

    #[test]
    fn budget_counts_settlements_and_stops_at_the_cap() {
        let budget = Budget::new(2);
        assert!(!budget.exhausted());
        assert_eq!(budget.record(), 1);
        assert!(!budget.exhausted());
        assert_eq!(budget.record(), 2);
        assert!(budget.exhausted());
    }

    #[test]
    fn a_zero_cap_refuses_before_any_payment() {
        assert!(Budget::new(0).exhausted());
    }

    /// Captured verbatim from a live v2 challenge (devnet, $0.20 USDC). The
    /// body of a 402 is empty, so this header is the whole diagnostic.
    const REAL_CHALLENGE: &str = "eyJ4NDAyVmVyc2lvbiI6MiwiZXJyb3IiOiJQYXltZW50LVNpZ25hdHVyZSBoZWFkZXIgaXMgcmVxdWlyZWQiLCJyZXNvdXJjZSI6eyJ1cmwiOiJodHRwOi8vMTI3LjAuMC4xOjgwODAvIn0sImFjY2VwdHMiOlt7InNjaGVtZSI6ImV4YWN0IiwibmV0d29yayI6InNvbGFuYTpFdFdUUkFCWmFZcTZpTWZlWUtvdVJ1MTY2VlUyeHFhMSIsImFtb3VudCI6IjIwMDAwMCIsInBheVRvIjoiN2JWa3RvUVJVYmRiWnhnb2VwcmdEdm1qQThrcm8zNUZMWkZIQjJ4ZDdjVlUiLCJtYXhUaW1lb3V0U2Vjb25kcyI6MzAwLCJhc3NldCI6IjR6TU1DOXNydDVSaTVYMTRHQWdYaGFIaWkzR25QQUVFUllQSmdaSkRuY0RVIiwiZXh0cmEiOnsiZmVlUGF5ZXIiOiJDN2NrRXpINHZhck1wQlFzYUQ5YkpaU0NuV1Z5azR6QUtZQTg1c3B1dU5iUiJ9fV19";

    #[test]
    fn a_real_challenge_reports_why_and_what_was_wanted() {
        let out = decode_challenge(REAL_CHALLENGE);
        assert!(
            out.contains("Payment-Signature header is required"),
            "{out}"
        );
        assert!(out.contains("200000"), "{out}");
        assert!(
            out.contains("solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1"),
            "{out}"
        );
        assert!(
            out.contains("7bVktoQRUbdbZxgoeprgDvmjA8kro35FLZFHB2xd7cVU"),
            "{out}"
        );
    }

    #[test]
    fn an_undecodable_challenge_still_says_something() {
        let out = decode_challenge("!!!not base64!!!");
        assert!(out.contains("undecodable"), "{out}");
    }

    #[test]
    fn valid_base64_that_is_not_json_falls_back_to_text() {
        use base64::Engine;
        let raw = base64::engine::general_purpose::STANDARD.encode("plain refusal text");
        assert_eq!(decode_challenge(&raw), "plain refusal text");
    }

    #[test]
    fn truncate_marks_where_it_cut() {
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("ab", 8), "ab");
    }
}
