//! OpenCode Go subscription usage query.

use std::fmt::Display;
use std::time::Duration;

use chrono::{DateTime, Local, TimeZone};
use reqwest::header::ACCEPT;
use serde_json::Value;

use crate::{APP, VERSION};

pub const USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";
const REQUEST_TIMEOUT_SECONDS: u64 = 30;
const MAX_ERROR_BODY_CHARS: usize = 1024;
const BAR_WIDTH: usize = 20;
const WINDOWS: [(&str, &str); 3] = [
    ("rolling", "Rolling (5h)"),
    ("weekly", "Weekly"),
    ("monthly", "Monthly"),
];

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
    #[error("usage response is missing a valid {0}")]
    Schema(&'static str),
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

pub fn render(usage: &Value) -> Result<String, UsageError> {
    render_in_timezone(usage, &Local)
}

fn render_in_timezone<Tz>(usage: &Value, timezone: &Tz) -> Result<String, UsageError>
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    let windows = usage
        .get("usage")
        .and_then(Value::as_object)
        .ok_or(UsageError::Schema("usage object"))?;
    let mut output = String::from("OpenCode Go usage\n");
    for (index, (key, label)) in WINDOWS.iter().enumerate() {
        let window = windows
            .get(*key)
            .and_then(Value::as_object)
            .ok_or(UsageError::Schema(key))?;
        let Some(percent) = window.get("percent").and_then(Value::as_f64) else {
            return Err(UsageError::Schema(key));
        };
        let percent_text = percent.to_string();
        let filled = ((percent.clamp(0.0, 100.0) / 100.0) * BAR_WIDTH as f64).round() as usize;
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(BAR_WIDTH - filled));
        let status = window
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let resets = window
            .get("resetsAt")
            .and_then(Value::as_str)
            .map(|timestamp| format_timestamp(timestamp, timezone))
            .unwrap_or_else(|| "unknown".to_string());

        if index > 0 {
            output.push('\n');
        }
        output.push_str(&format!(
            "{label:<12} [{bar}] {percent_text:>6}% used\n{:13}status: {status} · resets: {resets}\n",
            "",
        ));
    }
    Ok(output)
}

fn format_timestamp<Tz>(timestamp: &str, timezone: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    DateTime::parse_from_rfc3339(timestamp)
        .map(|time| {
            time.with_timezone(timezone)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| timestamp.to_string())
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use chrono::FixedOffset;
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

    #[test]
    fn renders_aligned_progress_bars_and_reset_times() {
        let usage = json!({
            "usage": {
                "rolling": {
                    "status": "ok",
                    "percent": 0,
                    "resetsAt": "2026-09-30T17:15:45.084Z"
                },
                "weekly": {
                    "status": "ok",
                    "percent": 50,
                    "resetsAt": "2026-10-05T00:00:00.000Z"
                },
                "monthly": {
                    "status": "ok",
                    "percent": 100,
                    "resetsAt": "2026-10-21T07:45:46.000Z"
                }
            }
        });

        let timezone = FixedOffset::east_opt(8 * 60 * 60).expect("UTC+8");
        assert_eq!(
            render_in_timezone(&usage, &timezone).expect("render"),
            concat!(
                "OpenCode Go usage\n",
                "Rolling (5h) [░░░░░░░░░░░░░░░░░░░░]      0% used\n",
                "             status: ok · resets: 2026-10-01 01:15:45\n",
                "\n",
                "Weekly       [██████████░░░░░░░░░░]     50% used\n",
                "             status: ok · resets: 2026-10-05 08:00:00\n",
                "\n",
                "Monthly      [████████████████████]    100% used\n",
                "             status: ok · resets: 2026-10-21 15:45:46\n",
            )
        );
    }

    #[test]
    fn clamps_out_of_range_percentages_to_the_bar_width() {
        let usage = json!({
            "usage": {
                "rolling": {"status": "ok", "percent": 125},
                "weekly": {"status": "ok", "percent": -20},
                "monthly": {"status": "ok", "percent": 50.5}
            }
        });
        let output = render(&usage).expect("render");
        assert!(output.contains("[████████████████████]    125% used"));
        assert!(output.contains("[░░░░░░░░░░░░░░░░░░░░]    -20% used"));
        assert!(output.contains("[██████████░░░░░░░░░░]   50.5% used"));
    }

    #[test]
    fn rejects_a_missing_usage_window() {
        let usage = json!({
            "usage": {
                "rolling": {"status": "ok", "percent": 1},
                "weekly": {"status": "ok", "percent": 2}
            }
        });
        assert!(matches!(render(&usage), Err(UsageError::Schema("monthly"))));
    }
}
