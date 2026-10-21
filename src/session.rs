//! Session-identifier extraction.
//!
//! Kimi Code already sends a stable per-conversation identifier in every
//! request body; the bridge reads it back out and republishes it as the
//! `x-opencode-session` header the upstream expects.

use serde_json::Value;

use crate::proxy::Style;

pub const SESSION_HEADER: &str = "x-opencode-session";

/// The distinct upstream routes the bridge serves, each with its own wire
/// protocol and its own error-body flavour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteKind {
    Models,
    OpenAi,
    OpenAiResponses,
    Anthropic,
    AnthropicCountTokens,
}

impl RouteKind {
    pub fn style(self) -> Style {
        match self {
            RouteKind::Anthropic | RouteKind::AnthropicCountTokens => Style::Anthropic,
            _ => Style::OpenAi,
        }
    }

    /// Client-facing explanation of what the bridge needed and did not get.
    pub fn hint(self) -> &'static str {
        match self {
            RouteKind::Anthropic | RouteKind::AnthropicCountTokens => {
                "missing or invalid Kimi session identifier: the request body must \
                 carry 'metadata.user_id' (string, 1-256 printable ASCII chars) so \
                 the bridge can route upstream KV cache per session"
            }
            _ => {
                "missing or invalid Kimi session identifier: the request body must \
                 carry a 'prompt_cache_key' field (string, 1-256 printable ASCII \
                 chars) so the bridge can route upstream KV cache per session"
            }
        }
    }
}

/// Accept only safe, non-empty header values as session identifiers.
///
/// The byte-length test is equivalent to Python's code-point `len()`: any
/// string that survives the printable-ASCII check below is one byte per
/// character, and any string that fails either test is rejected either way.
pub fn stable_session_id(value: &Value) -> Option<&str> {
    let text = value.as_str()?;
    if !(1..=256).contains(&text.len()) {
        return None;
    }
    if !text.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        return None;
    }
    Some(text)
}

/// Extract the Kimi conversation identifier from the request body.
pub fn session_id_from_body(kind: RouteKind, body: &Value) -> Option<&str> {
    let value = match kind {
        RouteKind::Anthropic | RouteKind::AnthropicCountTokens => body
            .get("metadata")
            .and_then(|metadata| metadata.get("user_id")),
        _ => body.get("prompt_cache_key"),
    };
    value.and_then(stable_session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_plain_printable_ascii() {
        assert_eq!(
            stable_session_id(&json!("test-session-A")),
            Some("test-session-A")
        );
        assert_eq!(stable_session_id(&json!("a")), Some("a"));
        // Both ends of the accepted printable range.
        assert_eq!(stable_session_id(&json!("!")), Some("!"));
        assert_eq!(stable_session_id(&json!("~")), Some("~"));
    }

    #[test]
    fn rejects_non_strings() {
        for value in [
            json!(1),
            json!(null),
            json!(true),
            json!(["a"]),
            json!({"a": 1}),
        ] {
            assert_eq!(stable_session_id(&value), None, "{value}");
        }
    }

    #[test]
    fn rejects_empty_and_overlong() {
        assert_eq!(stable_session_id(&json!("")), None);
        let max = "a".repeat(256);
        assert_eq!(stable_session_id(&json!(max)), Some(max.as_str()));
        assert_eq!(stable_session_id(&json!("a".repeat(257))), None);
    }

    #[test]
    fn rejects_unprintable_and_non_ascii() {
        for value in [
            json!("a b"),    // space is below 0x21
            json!("a\tb"),   // tab
            json!("a\nb"),   // newline would split the header
            json!("a\rb"),   // carriage return
            json!("\u{7f}"), // DEL
            json!("会话"),   // non-ASCII
            json!("café"),
        ] {
            assert_eq!(stable_session_id(&value), None, "{value}");
        }
    }

    #[test]
    fn openai_kinds_read_prompt_cache_key() {
        let body = json!({"prompt_cache_key": "sess-1", "model": "m"});
        assert_eq!(
            session_id_from_body(RouteKind::OpenAi, &body),
            Some("sess-1")
        );
        assert_eq!(
            session_id_from_body(RouteKind::OpenAiResponses, &body),
            Some("sess-1")
        );
    }

    #[test]
    fn openai_kinds_ignore_metadata_user_id() {
        let body = json!({"metadata": {"user_id": "sess-1"}});
        assert_eq!(session_id_from_body(RouteKind::OpenAi, &body), None);
    }

    #[test]
    fn anthropic_kinds_read_metadata_user_id() {
        let body = json!({"metadata": {"user_id": "sess-2"}});
        assert_eq!(
            session_id_from_body(RouteKind::Anthropic, &body),
            Some("sess-2")
        );
        assert_eq!(
            session_id_from_body(RouteKind::AnthropicCountTokens, &body),
            Some("sess-2")
        );
    }

    #[test]
    fn anthropic_kinds_reject_non_object_metadata() {
        for body in [
            json!({"metadata": "sess-2"}),
            json!({"metadata": ["sess-2"]}),
            json!({"metadata": null}),
            json!({"metadata": {"user_id": 7}}),
            json!({}),
        ] {
            assert_eq!(
                session_id_from_body(RouteKind::Anthropic, &body),
                None,
                "{body}"
            );
        }
    }

    #[test]
    fn missing_field_is_none() {
        assert_eq!(session_id_from_body(RouteKind::OpenAi, &json!({})), None);
        assert_eq!(
            session_id_from_body(RouteKind::OpenAi, &json!({"prompt_cache_key": null})),
            None
        );
    }
}
