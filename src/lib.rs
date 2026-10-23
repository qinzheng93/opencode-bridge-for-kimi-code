//! Kimi Code -> OpenCode Go bridge.
//!
//! A small relay that lets Kimi Code use an OpenCode Go subscription:
//!
//!   * Publishes its own API keys (a list in the config). Clients authenticate
//!     against the bridge with any key from that list; the bridge swaps in the
//!     real OpenCode Go key when talking to the upstream, so the upstream key
//!     never leaves the config file (or the `OPENCODE_BRIDGE_UPSTREAM_API_KEY`
//!     environment override).
//!   * Injects a stable `x-opencode-session` header on every inference
//!     request, derived per-request from fields Kimi Code already sends in the
//!     body (`prompt_cache_key` for OpenAI-style routes, `metadata.user_id`
//!     for the Anthropic route). The mapping is stateless, so it survives
//!     bridge restarts and keeps concurrent sessions isolated -- which is what
//!     makes the upstream per-session KV cache routing keep working.
//!   * Forwards request bodies byte-for-byte and streams SSE responses through
//!     without buffering.
//!   * Serves a models.dev-style registry at `/registry/api.json` for
//!     `kimi provider add`.
//!
//! Routes (all require the bridge API key unless noted):
//!   GET  /healthz                       liveness probe (no auth)
//!   GET  /registry/api.json             Kimi Code model registry (no auth)
//!   GET  /v1/models                     upstream model list, passthrough
//!   POST /v1/chat/completions           OpenAI wire protocol
//!   POST /v1/responses                  OpenAI Responses wire protocol
//!   POST /v1/messages                   Anthropic wire protocol
//!   POST /v1/messages/count_tokens      passthrough (no session header)
//! Every route above also exists under the `/zen/go/v1` prefix.
//!
//! Configuration is YAML; see config.example.yaml for an annotated template.
//! The config file is created with defaults on first run. Set
//! `upstream_api_key` (or export `OPENCODE_BRIDGE_UPSTREAM_API_KEY`) and
//! restart (or SIGHUP). One entry in `proxy_api_keys` is generated
//! automatically on first run and printed at startup; add more keys to the
//! list to hand out to more clients. The keys in that list are what Kimi Code
//! should be configured with.
//!
//! `--registry-protocols` restricts what `/registry/api.json` publishes, so
//! `kimi provider add` imports only the selected wire protocol(s) instead of
//! all three.

pub mod app;
pub mod config;
pub mod daemon;
pub mod keygen;
pub mod proxy;
pub mod registry;
pub mod session;

pub const APP: &str = "ocbk";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// reqwest's `Display` stops at "error sending request for url (...)", which
/// hides the cause a reader actually needs (connection reset, unknown issuer,
/// DNS failure). Append the `source()` chain.
pub fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !text.is_empty() && !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}
