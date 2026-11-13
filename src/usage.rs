//! OpenCode Go subscription usage query.

use std::time::Duration;

use reqwest::header::ACCEPT;
use serde_json::Value;

use crate::{APP, VERSION};

pub const USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";
const REQUEST_TIMEOUT_SECONDS: u64 = 30;
const MAX_ERROR_BODY_CHARS: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    #[error("cannot build the usage HTTP client: {0}")]
    Build(#[source] reqwest::Error),
    #[error("usage request failed: {0}")]
    Request(String),
    #[error("usage endpoint returned HTTP {status}: {body}")]
    Http {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("usage endpoint returned invalid JSON: {0}")]
    Json(#[source] serde_json::Error),
}

pub async fn fetch(api_key: &str) -> Result<Value, UsageError> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECONDS))
        .build()
        .map_err(UsageError::Build)?;
    fetch_from(&client, USAGE_URL, api_key).await
}

async fn fetch_from(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
) -> Result<Value, UsageError> {
    let response = client
        .get(url)
        .header(ACCEPT, "application/json")
        .header("User-Agent", format!("{APP}/{VERSION}"))
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|error| UsageError::Request(crate::error_chain(&error)))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| UsageError::Request(crate::error_chain(&error)))?;
    if !status.is_success() {
        let body = String::from_utf8_lossy(&body);
        let body = body.trim().chars().take(MAX_ERROR_BODY_CHARS).collect();
        return Err(UsageError::Http { status, body });
    }
    serde_json::from_slice(&body).map_err(UsageError::Json)
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use serde_json::json;

    use super::*;

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn fetches_usage_with_the_current_bearer_key() {
        async fn usage(headers: HeaderMap) -> axum::Json<Value> {
            assert_eq!(
                headers
                    .get(reqwest::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok()),
                Some("Bearer current-key")
            );
            assert_eq!(
                headers.get(ACCEPT).and_then(|value| value.to_str().ok()),
                Some("application/json")
            );
            axum::Json(json!({
                "usage": {
                    "rolling": {"status": "ok", "percent": 1},
                    "weekly": {"status": "ok", "percent": 25},
                    "monthly": {"status": "ok", "percent": 29}
                }
            }))
        }

        let base = serve(Router::new().route("/zen/go/v1/usage", get(usage))).await;
        let value = fetch_from(
            &reqwest::Client::new(),
            &format!("{base}/zen/go/v1/usage"),
            "current-key",
        )
        .await
        .expect("usage");
        assert_eq!(value["usage"]["weekly"]["percent"], 25);
    }

    #[tokio::test]
    async fn reports_non_success_responses_with_their_body() {
        async fn denied() -> (StatusCode, &'static str) {
            (StatusCode::UNAUTHORIZED, r#"{"error":"invalid key"}"#)
        }

        let base = serve(Router::new().route("/usage", get(denied))).await;
        let error = fetch_from(&reqwest::Client::new(), &format!("{base}/usage"), "bad-key")
            .await
            .expect_err("401");
        assert!(matches!(
            error,
            UsageError::Http {
                status: StatusCode::UNAUTHORIZED,
                ref body,
            } if body == r#"{"error":"invalid key"}"#
        ));
    }

    #[tokio::test]
    async fn rejects_invalid_success_json() {
        async fn invalid() -> &'static str {
            "not json"
        }

        let base = serve(Router::new().route("/usage", get(invalid))).await;
        let error = fetch_from(&reqwest::Client::new(), &format!("{base}/usage"), "key")
            .await
            .expect_err("invalid JSON");
        assert!(matches!(error, UsageError::Json(_)));
    }
}
