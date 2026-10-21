//! HTTP surface: routing, authentication, and the two local endpoints.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use serde_json::Value;
use subtle::ConstantTimeEq;

use crate::config::{BridgeConfig, ConfigStore};
use crate::proxy::{
    error_response, presented_api_key, BridgeError, RelayRequest, Style, UpstreamRelay,
};
use crate::registry::{Registry, Wire};
use crate::session::{session_id_from_body, RouteKind};

type Route = (&'static str, RouteKind);

#[derive(Clone)]
struct AppState {
    store: ConfigStore,
    relay: Arc<UpstreamRelay>,
    registry: Registry,
}

/// The five proxied routes, served under both `/v1` and `/zen/go/v1`.
fn classify(method: &Method, path: &str) -> Option<Route> {
    let rest = path
        .strip_prefix("/zen/go/v1")
        .or_else(|| path.strip_prefix("/v1"))?;
    match (method, rest) {
        (&Method::GET, "/models") => Some(("models", RouteKind::Models)),
        (&Method::POST, "/chat/completions") => Some(("chat/completions", RouteKind::OpenAi)),
        (&Method::POST, "/responses") => Some(("responses", RouteKind::OpenAiResponses)),
        (&Method::POST, "/messages") => Some(("messages", RouteKind::Anthropic)),
        (&Method::POST, "/messages/count_tokens") => {
            Some(("messages/count_tokens", RouteKind::AnthropicCountTokens))
        }
        _ => None,
    }
}

pub fn create_app(
    store: ConfigStore,
    relay: Arc<UpstreamRelay>,
    registry_protocols: Option<BTreeSet<Wire>>,
    public_base_url: String,
) -> Router {
    let state = AppState {
        relay,
        registry: Registry::new(store.clone(), registry_protocols, public_base_url),
        store,
    };
    Router::new()
        // One catch-all for every method: unknown paths *and* known paths with
        // the wrong method both have to answer 404, which a per-method router
        // would answer with 405 instead.
        .route("/{*path}", any(dispatch))
        .fallback(dispatch)
        .with_state(state)
}

async fn dispatch(State(state): State<AppState>, request: Request) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();

    if method == Method::GET && path == "/healthz" {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from("ok\n"))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }

    if method == Method::GET && path == "/registry/api.json" {
        return registry_endpoint(&state).await;
    }

    let Some((suffix, kind)) = classify(&method, &path) else {
        return error_response(
            StatusCode::NOT_FOUND,
            &format!("not found: {path}"),
            Style::OpenAi,
        );
    };

    match handle(&state, request, suffix, kind).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn registry_endpoint(state: &AppState) -> Response {
    let Some(client) = state.relay.client() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "registry unavailable: relay client is not started",
            Style::OpenAi,
        );
    };
    match state.registry.get(&client).await {
        Ok(data) => match serde_json::to_vec(&data) {
            Ok(body) => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CACHE_CONTROL, "no-store")
                .body(Body::from(body))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
            Err(error) => error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("registry unavailable: {error}"),
                Style::OpenAi,
            ),
        },
        Err(error) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("registry unavailable: {error}"),
            Style::OpenAi,
        ),
    }
}

async fn handle(
    state: &AppState,
    request: Request,
    suffix: &'static str,
    kind: RouteKind,
) -> Result<Response, BridgeError> {
    let style = kind.style();
    let cfg = state.store.get();
    authorize(&cfg, request.headers(), style)?;

    let (parts, body) = request.into_parts();
    let query = parts.uri.query().map(str::to_string);
    let headers = parts.headers;
    let method = parts.method;

    if kind == RouteKind::Models {
        return Ok(state
            .relay
            .relay(RelayRequest {
                method,
                headers,
                query,
                suffix,
                body: None,
                session: None,
                model: None,
                style,
            })
            .await);
    }

    let too_large = || {
        BridgeError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body too large",
            style,
        )
    };

    if let Some(declared) = declared_length(&headers) {
        if declared > cfg.max_body_bytes {
            return Err(too_large());
        }
    }

    let limit = cfg.max_body_bytes.saturating_add(1);
    let bytes = to_bytes(body, limit).await.map_err(|_| too_large())?;
    if bytes.len() > cfg.max_body_bytes {
        return Err(too_large());
    }

    if kind == RouteKind::AnthropicCountTokens {
        return Ok(state
            .relay
            .relay(RelayRequest {
                method,
                headers,
                query,
                suffix,
                body: Some(bytes),
                session: None,
                model: None,
                style,
            })
            .await);
    }

    let not_an_object = || {
        BridgeError::new(
            StatusCode::BAD_REQUEST,
            "request body must be a JSON object",
            style,
        )
    };
    let parsed: Value = serde_json::from_slice(&bytes).map_err(|_| not_an_object())?;
    if !parsed.is_object() {
        return Err(not_an_object());
    }

    let session = session_id_from_body(kind, &parsed)
        .ok_or_else(|| BridgeError::new(StatusCode::BAD_REQUEST, kind.hint(), style))?;
    let model = parsed
        .get("model")
        .and_then(Value::as_str)
        .map(|model| model.chars().take(64).collect::<String>());

    Ok(state
        .relay
        .relay(RelayRequest {
            method,
            headers,
            query,
            suffix,
            body: Some(bytes),
            session: Some(session.to_string()),
            model,
            style,
        })
        .await)
}

fn declared_length(headers: &HeaderMap) -> Option<usize> {
    let raw = headers.get(header::CONTENT_LENGTH)?.to_str().ok()?;
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

fn authorize(cfg: &BridgeConfig, headers: &HeaderMap, style: Style) -> Result<(), BridgeError> {
    if cfg.proxy_api_keys.is_empty() {
        return Err(BridgeError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "proxy_api_keys is not configured",
            style,
        ));
    }
    let presented = presented_api_key(headers);
    let matches = presented.is_some_and(|presented| {
        // `ct_eq` answers 0 for mismatched lengths, so this stays
        // length-safe the way `hmac.compare_digest` is.
        cfg.proxy_api_keys
            .iter()
            .any(|key| bool::from(presented.as_bytes().ct_eq(key.as_bytes())))
    });
    if !matches {
        return Err(BridgeError::new(
            StatusCode::UNAUTHORIZED,
            "invalid or missing API key: authenticate with any bridge API key \
             via 'Authorization: Bearer <key>' or 'x-api-key: <key>'",
            style,
        ));
    }
    Ok(())
}
