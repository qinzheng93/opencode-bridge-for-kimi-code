//! Upstream relay: credential swap, session injection, streaming passthrough.

use std::pin::Pin;
use std::sync::RwLock;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::Stream;
use serde_json::json;

use crate::config::ConfigStore;
use crate::session::SESSION_HEADER;

/// Mirrors the original's httpx timeouts. reqwest exposes no write or
/// pool-wait timeout, so the 120 s write and 30 s pool values have no
/// counterpart here; requests stay bounded by the connect and read timeouts.
pub const CONNECT_TIMEOUT_SECONDS: f64 = 10.0;
const MAX_IDLE_PER_HOST: usize = 16;

/// Error-body flavour: which of the two client protocols to speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    OpenAi,
    Anthropic,
}

#[derive(Debug)]
pub struct BridgeError {
    pub status: StatusCode,
    pub message: String,
    pub style: Style,
}

impl BridgeError {
    pub fn new(status: StatusCode, message: impl Into<String>, style: Style) -> Self {
        Self {
            status,
            message: message.into(),
            style,
        }
    }
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status.as_u16(), self.message)
    }
}

impl std::error::Error for BridgeError {}

impl IntoResponse for BridgeError {
    fn into_response(self) -> Response {
        error_response(self.status, &self.message, self.style)
    }
}

pub fn error_response(status: StatusCode, message: &str, style: Style) -> Response {
    let payload = match style {
        Style::Anthropic => json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": message},
        }),
        Style::OpenAi => json!({
            "error": {"message": message, "type": "invalid_request_error"},
        }),
    };
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json");
    if status == StatusCode::UNAUTHORIZED {
        builder = builder.header(header::WWW_AUTHENTICATE, "Bearer realm=\"opencode-bridge\"");
    }
    let body = serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec());
    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Read the client's bridge API key, in the original's precedence order.
pub fn presented_api_key(headers: &HeaderMap) -> Option<String> {
    if let Some(auth) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        let trimmed = auth.trim_start();
        let split = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        let (scheme, rest) = trimmed.split_at(split);
        if scheme.eq_ignore_ascii_case("bearer") {
            let token = rest.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    for name in ["x-api-key", "api-key"] {
        if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Connection framing headers, plus the client credentials the bridge
/// replaces and the session header the bridge is the only writer of.
const STRIP_REQUEST_HEADERS: [&str; 15] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
    "expect",
    "authorization",
    "x-api-key",
    "api-key",
    SESSION_HEADER,
];

const STRIP_RESPONSE_HEADERS: [&str; 10] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "server",
    "date",
];

fn connection_tokens(connection: Option<&HeaderValue>) -> Vec<String> {
    let Some(value) = connection.and_then(|value| value.to_str().ok()) else {
        return Vec::new();
    };
    value
        .split(',')
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect()
}

struct ClientState {
    client: reqwest::Client,
    read_timeout: f64,
}

/// Pooled HTTP client that forwards requests to OpenCode Go.
///
/// The request body is forwarded byte-for-byte; the only mutations are the
/// credential swap and the session header, both per-request constants that
/// cannot perturb upstream KV cache routing.
pub struct UpstreamRelay {
    store: ConfigStore,
    state: RwLock<Option<ClientState>>,
}

impl UpstreamRelay {
    pub fn new(store: ConfigStore) -> Self {
        Self {
            store,
            state: RwLock::new(None),
        }
    }

    pub fn start(&self, read_timeout_seconds: f64) -> Result<(), reqwest::Error> {
        let state = ClientState {
            client: build_client(read_timeout_seconds)?,
            read_timeout: read_timeout_seconds,
        };
        *self
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(state);
        Ok(())
    }

    /// `reqwest::Client` is internally reference counted, so cloning per
    /// request is cheap and gives a stable handle.
    pub fn client(&self) -> Option<reqwest::Client> {
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|state| state.client.clone())
    }

    /// Rebuild the pool when the configured read timeout changes on reload:
    /// reqwest has no per-request idle timeout, and `timeout()` is a total
    /// deadline that would cut long SSE streams short.
    pub fn apply_read_timeout(&self, seconds: f64) -> Result<(), reqwest::Error> {
        let mut guard = self
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let unchanged = guard
            .as_ref()
            .is_some_and(|state| (state.read_timeout - seconds).abs() < f64::EPSILON);
        if unchanged {
            return Ok(());
        }
        *guard = Some(ClientState {
            client: build_client(seconds)?,
            read_timeout: seconds,
        });
        Ok(())
    }

    pub async fn relay(&self, request: RelayRequest) -> Response {
        let cfg = self.store.get();
        let Some(client) = self.client() else {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "relay client is not started",
                request.style,
            );
        };

        let upstream_key = cfg.effective_upstream_api_key();
        if upstream_key.is_empty() {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "upstream_api_key is not configured in the bridge config",
                request.style,
            );
        }

        let headers = match self.upstream_headers(
            &request.headers,
            &upstream_key,
            request.session.as_deref(),
            request.style,
        ) {
            Ok(headers) => headers,
            Err(error) => return error.into_response(),
        };

        let path = format!("{}/{}", cfg.upstream_prefix(), request.suffix);
        let mut url = format!("{}{}", cfg.upstream_base_url, path);
        if let Some(query) = request.query.as_deref() {
            if !query.is_empty() {
                url.push('?');
                url.push_str(query);
            }
        }

        let upstream = client
            .request(request.method.clone(), url)
            .headers(headers)
            .body(request.body.unwrap_or_default())
            .send()
            .await;

        let upstream = match upstream {
            Ok(response) => response,
            Err(error) => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("upstream request failed: {}", crate::error_chain(&error)),
                    request.style,
                )
            }
        };

        let status = upstream.status();
        let response_tokens = connection_tokens(upstream.headers().get(header::CONNECTION));
        let mut response_headers = HeaderMap::new();
        for (name, value) in upstream.headers() {
            let lowered = name.as_str();
            if STRIP_RESPONSE_HEADERS.contains(&lowered)
                || response_tokens
                    .iter()
                    .any(|token| token.as_str() == lowered)
            {
                continue;
            }
            response_headers.append(name.clone(), value.clone());
        }

        let stream = CountingStream {
            inner: Box::pin(upstream.bytes_stream()),
            total: 0,
            context: Some(RelayLog {
                method: request.method,
                path,
                session: request.session,
                model: request.model,
                status,
            }),
        };

        let mut builder = Response::builder().status(status.as_u16());
        if let Some(map) = builder.headers_mut() {
            *map = response_headers;
        }
        builder.body(Body::from_stream(stream)).unwrap_or_else(|_| {
            error_response(
                StatusCode::BAD_GATEWAY,
                "invalid upstream response",
                request.style,
            )
        })
    }

    fn upstream_headers(
        &self,
        headers: &HeaderMap,
        upstream_key: &str,
        session: Option<&str>,
        style: Style,
    ) -> Result<HeaderMap, BridgeError> {
        let tokens = connection_tokens(headers.get(header::CONNECTION));
        let mut out = HeaderMap::new();
        for (name, value) in headers {
            let lowered = name.as_str();
            if STRIP_REQUEST_HEADERS.contains(&lowered) || tokens.iter().any(|t| t == lowered) {
                continue;
            }
            out.append(name.clone(), value.clone());
        }

        let authorization =
            HeaderValue::from_str(&format!("Bearer {upstream_key}")).map_err(|_| {
                BridgeError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "upstream_api_key contains characters that cannot be sent in a header",
                    style,
                )
            })?;
        out.insert(header::AUTHORIZATION, authorization);

        if let Some(session) = session {
            if let Ok(value) = HeaderValue::from_str(session) {
                out.insert(HeaderName::from_static(SESSION_HEADER), value);
            }
        }
        Ok(out)
    }
}

struct RelayLog {
    method: Method,
    path: String,
    session: Option<String>,
    model: Option<String>,
    status: StatusCode,
}

/// Wraps the upstream byte stream so the relay can log one summary line when
/// the stream ends (or the client goes away), without ever buffering a chunk.
struct CountingStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    total: u64,
    context: Option<RelayLog>,
}

impl CountingStream {
    fn finish(&mut self) {
        let Some(context) = self.context.take() else {
            return;
        };
        let session = context.session.as_deref().unwrap_or("-");
        let model = context.model.as_deref().unwrap_or("-");
        if context.status.as_u16() < 500 {
            tracing::info!(
                "{} {} session={} model={} -> {} ({} bytes)",
                context.method,
                context.path,
                session,
                model,
                context.status.as_u16(),
                self.total
            );
        } else {
            tracing::warn!(
                "{} {} session={} model={} -> {} ({} bytes)",
                context.method,
                context.path,
                session,
                model,
                context.status.as_u16(),
                self.total
            );
        }
    }
}

impl Stream for CountingStream {
    type Item = Result<Bytes, reqwest::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                this.finish();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(error))) => {
                this.finish();
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(Some(Ok(chunk))) => {
                this.total += chunk.len() as u64;
                Poll::Ready(Some(Ok(chunk)))
            }
        }
    }
}

impl Drop for CountingStream {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Everything the relay needs about one client request.
pub struct RelayRequest {
    pub method: Method,
    pub headers: HeaderMap,
    pub query: Option<String>,
    pub suffix: &'static str,
    pub body: Option<Bytes>,
    pub session: Option<String>,
    pub model: Option<String>,
    pub style: Style,
}

fn build_client(read_timeout_seconds: f64) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        // httpx's trust_env=False: never pick up proxy settings from the env.
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs_f64(CONNECT_TIMEOUT_SECONDS))
        // An idle timeout between chunks, which is what keeps long SSE
        // streams alive; `timeout()` would be a total deadline instead.
        .read_timeout(Duration::from_secs_f64(read_timeout_seconds))
        .pool_max_idle_per_host(MAX_IDLE_PER_HOST)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                HeaderValue::from_str(value).expect("header value"),
            );
        }
        map
    }

    #[test]
    fn reads_bearer_then_falls_back_to_api_key_headers() {
        assert_eq!(
            presented_api_key(&headers(&[("authorization", "Bearer abc")])),
            Some("abc".to_string())
        );
        assert_eq!(
            presented_api_key(&headers(&[("authorization", "bearer abc")])),
            Some("abc".to_string())
        );
        assert_eq!(
            presented_api_key(&headers(&[("authorization", "Bearer   abc  ")])),
            Some("abc".to_string())
        );
        assert_eq!(
            presented_api_key(&headers(&[("authorization", "Bearer a b")])),
            Some("a b".to_string())
        );
        assert_eq!(
            presented_api_key(&headers(&[("x-api-key", "abc")])),
            Some("abc".to_string())
        );
        assert_eq!(
            presented_api_key(&headers(&[("api-key", "abc")])),
            Some("abc".to_string())
        );
        assert_eq!(
            presented_api_key(&headers(&[("x-api-key", "abc"), ("api-key", "def")])),
            Some("abc".to_string())
        );
    }

    #[test]
    fn ignores_absent_or_unusable_credentials() {
        assert_eq!(presented_api_key(&headers(&[])), None);
        assert_eq!(presented_api_key(&headers(&[("authorization", "")])), None);
        assert_eq!(
            presented_api_key(&headers(&[("authorization", "Bearer")])),
            None
        );
        assert_eq!(
            presented_api_key(&headers(&[("authorization", "Bearer ")])),
            None
        );
        assert_eq!(
            presented_api_key(&headers(&[("authorization", "Basic abc")])),
            None
        );
        assert_eq!(presented_api_key(&headers(&[("x-api-key", "")])), None);
    }

    #[test]
    fn builds_the_expected_error_bodies() {
        let openai = error_response(StatusCode::BAD_REQUEST, "nope", Style::OpenAi);
        assert_eq!(openai.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            openai.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let anthropic = error_response(StatusCode::NOT_FOUND, "nope", Style::Anthropic);
        assert_eq!(
            anthropic.headers().get(header::WWW_AUTHENTICATE),
            None,
            "the challenge is only for 401"
        );
        let unauthorized = error_response(StatusCode::UNAUTHORIZED, "nope", Style::OpenAi);
        assert_eq!(
            unauthorized
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .unwrap(),
            "Bearer realm=\"opencode-bridge\""
        );
    }

    #[test]
    fn connection_tokens_are_lowercased_and_trimmed() {
        assert_eq!(
            connection_tokens(Some(&HeaderValue::from_static("Keep-Alive, X-Custom"))),
            vec!["keep-alive".to_string(), "x-custom".to_string()]
        );
        assert!(connection_tokens(None).is_empty());
        assert!(connection_tokens(Some(&HeaderValue::from_static(""))).is_empty());
    }

    #[test]
    fn strip_lists_cover_the_documented_headers() {
        for name in [
            "host",
            "content-length",
            "expect",
            "authorization",
            "x-api-key",
            "api-key",
            SESSION_HEADER,
            "transfer-encoding",
        ] {
            assert!(STRIP_REQUEST_HEADERS.contains(&name), "{name}");
        }
        for name in ["server", "date", "transfer-encoding", "connection"] {
            assert!(STRIP_RESPONSE_HEADERS.contains(&name), "{name}");
        }
    }
}
