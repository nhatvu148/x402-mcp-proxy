//! A tiny x402-gated MCP server that has nothing to do with video, whisper, or
//! any other project.
//!
//! It exists to answer one question: *is `x402-mcp-proxy` actually generic, or
//! is it secretly coupled to the server it was written against?* This shares no
//! code with that server — no `rmcp`, no shared crate, no copied module. MCP is
//! spoken by hand below, in about a hundred lines, which is all the protocol
//! this needs.
//!
//! Two tools:
//!   - `ping`      free   — so an agent can discover the server without paying
//!   - `flip_coin` $0.01  — the paid one
//!
//! Free discovery, paid execution: `initialize` and `tools/list` must stay free
//! or an agent can never read the catalogue to decide whether to buy.
//!
//! Run:
//!   X402_PAY_TO=<your-solana-address> cargo run -p coinflip-server
//!   # then point the proxy at http://127.0.0.1:8921/mcp
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tower::Service;
use x402_axum::X402Middleware;
use x402_chain_solana::{KnownNetworkSolana, V2SolanaExact};
use x402_types::networks::USDC;

const PRICE_USD: &str = "0.01";

/// Tools this server charges for. Everything else is free.
const PRICED_TOOLS: &[&str] = &["flip_coin"];

#[tokio::main]
async fn main() {
    let pay_to = std::env::var("X402_PAY_TO").unwrap_or_default();
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        // NOT 8899: that is Solana's default RPC port, so a local validator or
        // a stray earlier run collides with it — which is exactly what
        // happened the first time this was demoed.
        .unwrap_or(8921);

    // The MCP handler, twice: once plain, once behind the payment layer.
    let free = tower::service_fn(handle);
    let paid_inner = tower::service_fn(handle);

    // `None` needs a concrete type, and the payment layer's is unnameable, so
    // the paid arm is boxed. Costs one allocation per request on a path that
    // already does a network round trip to a facilitator.
    type Paid = tower::util::BoxCloneSyncService<
        Request<Body>,
        Response,
        std::convert::Infallible,
    >;

    let app = if pay_to.trim().is_empty() {
        eprintln!("X402_PAY_TO unset — running free, nothing will be charged");
        axum::Router::new().fallback_service(McpRouter {
            free: free.clone(),
            paid: None::<Paid>,
        })
    } else {
        let addr: x402_chain_solana::chain::Address =
            pay_to.trim().parse().expect("X402_PAY_TO is not a Solana address");
        let usdc = USDC::solana_devnet();
        let amount = usdc.parse(PRICE_USD).expect("price");

        eprintln!("payments ON — ${PRICE_USD} USDC per flip_coin, paid to {pay_to}");

        let layer = X402Middleware::new("https://facilitator.x402.rs")
            // Settle first. A signed Solana transaction dies with its blockhash
            // after ~60-90s; a coin flip is instant so it hardly matters here,
            // but it matches how a real server has to work.
            .settle_before_execution()
            .with_price_tag(V2SolanaExact::price_tag(addr, amount));

        let paid: Paid = tower::util::BoxCloneSyncService::new(
            tower::Layer::layer(&layer, paid_inner),
        );
        axum::Router::new().fallback_service(McpRouter {
            free: free.clone(),
            paid: Some(paid),
        })
    };

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind");
    eprintln!("coinflip MCP server on http://127.0.0.1:{port}/mcp");
    axum::serve(listener, app).await.expect("serve");
}

/// Routes each request to the free or paid service after peeking at the
/// JSON-RPC method — MCP puts every method behind one POST, so the payment
/// layer can't decide on the URL alone.
#[derive(Clone)]
struct McpRouter<F, P> {
    free: F,
    paid: Option<P>,
}

impl<F, P> Service<Request<Body>> for McpRouter<F, P>
where
    F: Service<Request<Body>, Response = Response> + Clone + Send + 'static,
    F::Future: Send + 'static,
    F::Error: Send + 'static,
    P: Service<Request<Body>, Response = Response, Error = F::Error> + Clone + Send + 'static,
    P::Future: Send + 'static,
{
    type Response = Response;
    type Error = F::Error;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let mut free = self.free.clone();
        let paid = self.paid.clone();
        Box::pin(async move {
            let (parts, body) = request.into_parts();
            let bytes = axum::body::to_bytes(body, 1024 * 1024)
                .await
                .unwrap_or_default();
            let priced = is_priced(&bytes);
            let request = Request::from_parts(parts, Body::from(bytes));

            match (priced, paid) {
                (true, Some(mut paid)) => paid.call(request).await,
                _ => free.call(request).await,
            }
        })
    }
}

/// True when the body is a `tools/call` for a tool we charge for. Anything else
/// — discovery, notifications, free tools, unparseable bodies — is free.
/// Failing open to *free* is deliberate: an unparseable request can't do
/// billable work either.
fn is_priced(body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    v.get("method").and_then(|m| m.as_str()) == Some("tools/call")
        && v.get("params")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .is_some_and(|n| PRICED_TOOLS.contains(&n))
}

/// The whole MCP surface: initialize, tools/list, tools/call.
async fn handle(request: Request<Body>) -> Result<Response, std::convert::Infallible> {
    let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .unwrap_or_default();
    let Ok(req) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(StatusCode::BAD_REQUEST.into_response());
    };
    let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

    // Notifications carry no id and expect no body back.
    if id.is_null() {
        return Ok((StatusCode::ACCEPTED, "").into_response());
    }

    let result = match method {
        "initialize" => serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "coinflip", "version": "0.1.0" },
            "instructions": "Flips a coin. Exists to prove x402-mcp-proxy works \
                             against a server it was not written for."
        }),
        "tools/list" => serde_json::json!({
            "tools": [
                {
                    "name": "ping",
                    "description": "Returns pong. Free — so an agent can check the \
                                    server is alive without paying.",
                    "inputSchema": { "type": "object", "properties": {} }
                },
                {
                    "name": "flip_coin",
                    "description": format!(
                        "Flip a coin and return heads or tails. \
                         COST: ${PRICE_USD} USDC per call (solana-devnet), paid via x402 — \
                         the server answers an unpaid call with HTTP 402 and payment \
                         instructions."
                    ),
                    "inputSchema": { "type": "object", "properties": {} }
                }
            ]
        }),
        "tools/call" => {
            let tool = req
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            match tool {
                "ping" => tool_text("pong (free)"),
                "flip_coin" => {
                    // uuid rather than rand: one fewer dependency, and the low
                    // bit of a v4 is as fair as this demo needs.
                    let heads = uuid::Uuid::new_v4().as_bytes()[0] % 2 == 0;
                    tool_text(if heads { "heads 🪙" } else { "tails 🪙" })
                }
                other => {
                    return Ok(json_rpc_error(id, -32601, &format!("unknown tool: {other}")));
                }
            }
        }
        other => return Ok(json_rpc_error(id, -32601, &format!("unknown method: {other}"))),
    };

    let mut response = axum::Json(serde_json::json!({
        "jsonrpc": "2.0", "id": id, "result": result
    }))
    .into_response();

    // The streamable-HTTP transport binds later requests to a session.
    if method == "initialize" {
        response.headers_mut().insert(
            "mcp-session-id",
            uuid::Uuid::new_v4().to_string().parse().unwrap(),
        );
    }
    response
        .headers_mut()
        .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    Ok(response)
}

fn tool_text(text: &str) -> serde_json::Value {
    serde_json::json!({ "content": [{ "type": "text", "text": text }], "isError": false })
}

fn json_rpc_error(id: serde_json::Value, code: i64, message: &str) -> Response {
    axum::Json(serde_json::json!({
        "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message }
    }))
    .into_response()
}
