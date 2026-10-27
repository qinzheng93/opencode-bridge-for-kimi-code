//! Model discovery: Go model roster + OpenCode metadata -> Kimi registry.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use reqwest::header::{ETAG, IF_NONE_MATCH};
use reqwest::StatusCode;
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::config::ConfigStore;
use crate::{APP, VERSION};

/// Catalog statuses upstream no longer promotes; Kimi Code's own catalog
/// imports exclude these, so the bridge does too.
const EXCLUDED_STATUSES: [&str; 2] = ["deprecated", "alpha"];

const MIN_TTL_SECONDS: u64 = 60;
/// The roster is small; a slow answer is not worth waiting for.
const ROSTER_TIMEOUT_SECONDS: u64 = 15;
/// The metadata catalog is several megabytes uncompressed on a cold cache.
const METADATA_TIMEOUT_SECONDS: u64 = 60;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct RegistryError(String);

impl RegistryError {
    fn message(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// One provider per wire protocol: Kimi Code providers speak exactly one
/// protocol each, so models are bucketed by their npm SDK metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Wire {
    OpenAi,
    OpenAiResponses,
    Anthropic,
}

const WIRES: [Wire; 3] = [Wire::OpenAi, Wire::OpenAiResponses, Wire::Anthropic];

impl Wire {
    fn from_npm(npm: &str) -> Option<Self> {
        match npm {
            "@ai-sdk/openai-compatible" => Some(Wire::OpenAi),
            "@ai-sdk/openai" => Some(Wire::OpenAiResponses),
            "@ai-sdk/anthropic" => Some(Wire::Anthropic),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Wire::OpenAi => "openai",
            Wire::OpenAiResponses => "openai_responses",
            Wire::Anthropic => "anthropic",
        }
    }

    fn provider(self) -> (&'static str, &'static str) {
        match self {
            Wire::OpenAi => ("opencode-go-chat", "OpenCode Go (Chat)"),
            Wire::OpenAiResponses => ("opencode-go-responses", "OpenCode Go (Responses)"),
            Wire::Anthropic => ("opencode-go-anthropic", "OpenCode Go (Anthropic)"),
        }
    }
}

/// Parse a `--registry-protocols` value; `None` means all protocols.
pub fn parse_protocol_selection(value: &str) -> Result<Option<BTreeSet<Wire>>, String> {
    let tokens: Vec<String> = value
        .split(',')
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.is_empty() {
        return Err("no protocols given".to_string());
    }
    if tokens.iter().any(|token| token == "all") {
        return Ok(None);
    }
    let mut selected = BTreeSet::new();
    for token in &tokens {
        let wire = match token.as_str() {
            "chat" | "openai" => Wire::OpenAi,
            "responses" | "openai_responses" => Wire::OpenAiResponses,
            "anthropic" => Wire::Anthropic,
            _ => {
                return Err(format!(
                    "unknown protocol {token:?}; expected chat, responses, anthropic, or all"
                ))
            }
        };
        selected.insert(wire);
    }
    Ok(Some(selected))
}

/// Every wire protocol, i.e. no filtering.
pub fn all_wires() -> BTreeSet<Wire> {
    WIRES.iter().copied().collect()
}

/// Model IDs authorized for the Go API key.
pub fn model_roster(payload: &Value) -> Result<BTreeSet<String>, RegistryError> {
    let rows = payload
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| RegistryError::message("unexpected upstream /models response"))?;

    let ids: BTreeSet<String> = rows
        .iter()
        .filter_map(|row| row.get("id").and_then(Value::as_str))
        .filter(|id| is_safe_model_id(id))
        .map(String::from)
        .collect();

    if ids.is_empty() {
        return Err(RegistryError::message(
            "upstream /models returned no valid model IDs",
        ));
    }
    Ok(ids)
}

/// The `^[A-Za-z0-9][A-Za-z0-9._-]*$` check the original expresses as a regex.
fn is_safe_model_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    match bytes.next() {
        Some(byte) if byte.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// Positive JSON integers, excluding booleans and non-integral numbers the way
/// Python's `isinstance(x, int) and not isinstance(x, bool)` does.
fn json_positive_int(value: &Value) -> Option<i64> {
    let number = match value {
        Value::Number(number) => number,
        _ => return None,
    };
    if let Some(value) = number.as_i64() {
        return (value > 0).then_some(value);
    }
    number
        .as_u64()
        .map(|value| i64::try_from(value).unwrap_or(i64::MAX))
        .filter(|value| *value > 0)
}

/// Extract thinking-effort levels from models.dev `reasoning_options`.
///
/// The object form (`{"type": "effort", "values": [...]}`) is what
/// models.opencode.ai serves today; a plain string array is the legacy form.
/// Kimi Code's custom-registry import ignores `reasoning_options`, so these
/// are re-emitted as the native `support_efforts` field.
pub fn effort_values(meta: &Map<String, Value>) -> Option<Vec<String>> {
    let options = meta.get("reasoning_options")?.as_array()?;
    for option in options {
        let Some(option) = option.as_object() else {
            continue;
        };
        if option.get("type").and_then(Value::as_str) != Some("effort") {
            continue;
        }
        let Some(values) = option.get("values").and_then(Value::as_array) else {
            continue;
        };
        if values.is_empty() {
            continue;
        }
        let collected: Option<Vec<String>> = values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .filter(|text| !text.is_empty())
                    .map(String::from)
            })
            .collect();
        if let Some(collected) = collected {
            return Some(collected);
        }
    }

    if options.is_empty() {
        return None;
    }
    options
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|text| !text.is_empty())
                .map(String::from)
        })
        .collect()
}

/// Convert the Go model roster plus OpenCode metadata into a Kimi registry.
///
/// `protocols` restricts the output to a subset of wires; models whose wire is
/// not selected are dropped and reported as a summary line in the returned
/// skip list.
pub fn build_registry(
    go_models: &Value,
    catalog: &Value,
    base_url: &str,
    env_name: &str,
    protocols: Option<&BTreeSet<Wire>>,
) -> Result<(Value, Vec<String>), RegistryError> {
    let ids = model_roster(go_models)?;

    let entry = catalog
        .get("opencode-go")
        .and_then(Value::as_object)
        .filter(|entry| entry.get("models").is_some_and(Value::is_object))
        .ok_or_else(|| RegistryError::message("catalog missing opencode-go.models"))?;
    let upstream_models = entry
        .get("models")
        .and_then(Value::as_object)
        .expect("checked above");

    let selected: BTreeSet<Wire> = match protocols {
        Some(protocols) => protocols.clone(),
        None => WIRES.iter().copied().collect(),
    };

    let mut registry = Map::new();
    for wire in WIRES {
        if !selected.contains(&wire) {
            continue;
        }
        let (provider_id, provider_name) = wire.provider();
        let mut provider = Map::new();
        provider.insert("id".to_string(), Value::from(provider_id));
        provider.insert("name".to_string(), Value::from(provider_name));
        provider.insert("type".to_string(), Value::from(wire.as_str()));
        // The bridge's own client-facing route (it also serves /v1/*);
        // independent of the upstream prefix, which is configurable.
        provider.insert(
            "api".to_string(),
            Value::from(format!("{base_url}/zen/go/v1")),
        );
        provider.insert("env".to_string(), Value::from(vec![env_name]));
        provider.insert("models".to_string(), Value::Object(Map::new()));
        registry.insert(provider_id.to_string(), Value::Object(provider));
    }

    let mut skipped: Vec<String> = Vec::new();
    let mut excluded: BTreeMap<String, usize> = BTreeMap::new();
    let mut excluded_status: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for model_id in &ids {
        let Some(meta) = upstream_models.get(model_id).and_then(Value::as_object) else {
            skipped.push(format!("{model_id}: no metadata"));
            continue;
        };

        if let Some(status) = meta
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase)
        {
            if EXCLUDED_STATUSES.contains(&status.as_str()) {
                excluded_status
                    .entry(status)
                    .or_default()
                    .push(model_id.clone());
                continue;
            }
        }

        let limit = meta.get("limit").and_then(Value::as_object);
        let context = limit
            .and_then(|limit| limit.get("context"))
            .and_then(json_positive_int);
        let Some(context) = context else {
            skipped.push(format!("{model_id}: missing context limit"));
            continue;
        };

        // Prefer a model-specific protocol override.
        let npm = meta
            .get("provider")
            .and_then(|provider| provider.get("npm"))
            .and_then(Value::as_str)
            .filter(|npm| !npm.is_empty())
            .or_else(|| entry.get("npm").and_then(Value::as_str));
        let Some(wire) = npm.and_then(Wire::from_npm) else {
            skipped.push(format!("{model_id}: unsupported npm protocol {npm:?}"));
            continue;
        };
        if !selected.contains(&wire) {
            *excluded.entry(wire.as_str().to_string()).or_insert(0) += 1;
            continue;
        }

        let mut clean_limit = Map::new();
        clean_limit.insert("context".to_string(), Value::from(context));
        for field in ["input", "output"] {
            let value = limit
                .and_then(|limit| limit.get(field))
                .and_then(json_positive_int);
            if let Some(value) = value {
                clean_limit.insert(field.to_string(), Value::from(value));
            }
        }

        let mut dest = Map::new();
        dest.insert("id".to_string(), Value::from(model_id.as_str()));
        dest.insert(
            "name".to_string(),
            Value::from(
                meta.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id.as_str()),
            ),
        );
        dest.insert("limit".to_string(), Value::Object(clean_limit));
        for field in ["reasoning", "tool_call"] {
            if let Some(Value::Bool(flag)) = meta.get(field) {
                dest.insert(field.to_string(), Value::from(*flag));
            }
        }
        if let Some(value @ Value::Object(_)) = meta.get("modalities") {
            dest.insert("modalities".to_string(), value.clone());
        }
        if let Some(value @ Value::Array(_)) = meta.get("reasoning_options") {
            dest.insert("reasoning_options".to_string(), value.clone());
        }
        if let Some(efforts) = effort_values(meta) {
            dest.insert("support_efforts".to_string(), Value::from(efforts));
        }
        if let Some(value) = meta.get("interleaved") {
            if value.is_boolean() || value.is_object() {
                dest.insert("interleaved".to_string(), value.clone());
            }
        }

        let (provider_id, _) = wire.provider();
        if let Some(Value::Object(provider)) = registry.get_mut(provider_id) {
            if let Some(Value::Object(models)) = provider.get_mut("models") {
                models.insert(model_id.clone(), Value::Object(dest));
            }
        }
    }

    for (wire, count) in &excluded {
        skipped.push(format!("protocol filter excluded {count} {wire} model(s)"));
    }
    for (status, model_ids) in &excluded_status {
        let sample = model_ids
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let more = if model_ids.len() > 5 {
            format!(" (+{} more)", model_ids.len() - 5)
        } else {
            String::new()
        };
        skipped.push(format!(
            "status filter excluded {} {status} model(s): {sample}{more}",
            model_ids.len()
        ));
    }

    // Omit providers that have no matching models.
    registry.retain(|_, provider| {
        provider
            .get("models")
            .and_then(Value::as_object)
            .is_some_and(|models| !models.is_empty())
    });

    if registry.is_empty() {
        let mut chosen: Vec<&str> = selected.iter().map(|wire| wire.as_str()).collect();
        chosen.sort_unstable();
        return Err(RegistryError::message(format!(
            "no importable models for the selected protocols ({}); check Go entitlement and metadata",
            chosen.join(", ")
        )));
    }

    Ok((Value::Object(registry), skipped))
}

/// A response body plus the validator needed to revalidate it later. `Bytes`
/// keeps the raw catalog so a 304 refresh costs no parsing when the roster has
/// not changed either.
#[derive(Clone)]
struct JsonBody {
    bytes: Bytes,
    etag: Option<String>,
}

impl JsonBody {
    fn parse(&self) -> Result<Value, RegistryError> {
        serde_json::from_slice(&self.bytes)
            .map_err(|error| RegistryError::message(error.to_string()))
    }
}

/// `None` means the server answered 304: the cached body is still current.
async fn fetch_json(
    client: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    etag: Option<&str>,
    timeout: Duration,
) -> Result<Option<JsonBody>, RegistryError> {
    let mut request = client
        .get(url)
        .header("Accept", "application/json")
        .header("User-Agent", format!("{APP}/{VERSION}"))
        .timeout(timeout);
    if let Some(bearer) = bearer.filter(|bearer| !bearer.is_empty()) {
        request = request.header("Authorization", format!("Bearer {bearer}"));
    }
    if let Some(etag) = etag {
        request = request.header(IF_NONE_MATCH, etag);
    }
    let response = request
        .send()
        .await
        .map_err(|error| RegistryError::message(crate::error_chain(&error)))?;
    if response.status() == StatusCode::NOT_MODIFIED {
        return Ok(None);
    }
    let response = response
        .error_for_status()
        .map_err(|error| RegistryError::message(crate::error_chain(&error)))?;
    let etag = response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(String::from);
    let bytes = response
        .bytes()
        .await
        .map_err(|error| RegistryError::message(crate::error_chain(&error)))?;
    Ok(Some(JsonBody { bytes, etag }))
}

struct CacheEntry {
    data: Value,
    catalog: JsonBody,
    roster: BTreeSet<String>,
    expires: Instant,
}

struct Refreshed {
    data: Value,
    catalog: JsonBody,
    roster: BTreeSet<String>,
    skipped: Vec<String>,
    not_modified: bool,
}

/// TTL-cached registry with stale-serve fallback on upstream failure.
#[derive(Clone)]
pub struct Registry {
    store: ConfigStore,
    protocols: Option<BTreeSet<Wire>>,
    /// Where clients reach the bridge. Startup-only, like the bound socket
    /// it is derived from.
    public_base_url: String,
    cache: Arc<Mutex<Option<CacheEntry>>>,
}

impl Registry {
    pub fn new(
        store: ConfigStore,
        protocols: Option<BTreeSet<Wire>>,
        public_base_url: String,
    ) -> Self {
        Self {
            store,
            protocols,
            public_base_url,
            cache: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn get(&self, client: &reqwest::Client) -> Result<Value, RegistryError> {
        if let Some(data) = self.fresh_cache() {
            return Ok(data);
        }

        let mut guard = self.cache.lock().await;
        if let Some(entry) = guard.as_ref() {
            if Instant::now() < entry.expires {
                return Ok(entry.data.clone());
            }
        }

        let cfg = self.store.get();
        let previous = guard.as_ref();
        match self.fetch(client, &cfg, previous).await {
            Ok(refreshed) => {
                let ttl = cfg.registry_ttl_seconds.max(MIN_TTL_SECONDS);
                *guard = Some(CacheEntry {
                    data: refreshed.data.clone(),
                    catalog: refreshed.catalog,
                    roster: refreshed.roster,
                    expires: Instant::now() + Duration::from_secs(ttl),
                });
                if refreshed.not_modified {
                    tracing::debug!("registry metadata not modified; reusing cached copy");
                } else {
                    let summary = match refreshed.data.as_object() {
                        Some(providers) => providers
                            .iter()
                            .map(|(name, provider)| {
                                let count = provider
                                    .get("models")
                                    .and_then(|models| models.as_object())
                                    .map_or(0, |models| models.len());
                                format!("{name}={count}")
                            })
                            .collect::<Vec<_>>()
                            .join(", "),
                        None => String::new(),
                    };
                    let sample = refreshed
                        .skipped
                        .iter()
                        .take(20)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("; ");
                    tracing::info!("registry refreshed: {summary}");
                    if !refreshed.skipped.is_empty() {
                        tracing::warn!("registry skipped models: {sample}");
                    }
                }
                Ok(refreshed.data)
            }
            Err(error) => {
                tracing::warn!(
                    "registry refresh failed ({}); serving cached copy if any",
                    error
                );
                if let Some(entry) = guard.as_mut() {
                    entry.expires = Instant::now() + Duration::from_secs(MIN_TTL_SECONDS);
                    return Ok(entry.data.clone());
                }
                Err(error)
            }
        }
    }

    fn fresh_cache(&self) -> Option<Value> {
        let guard = self.cache.try_lock().ok()?;
        let entry = guard.as_ref()?;
        (Instant::now() < entry.expires).then(|| entry.data.clone())
    }

    async fn fetch(
        &self,
        client: &reqwest::Client,
        cfg: &crate::config::BridgeConfig,
        previous: Option<&CacheEntry>,
    ) -> Result<Refreshed, RegistryError> {
        let roster_url = format!("{}{}/models", cfg.upstream_base_url, cfg.upstream_prefix());
        let upstream_key = cfg.effective_upstream_api_key();
        let roster_timeout = Duration::from_secs(ROSTER_TIMEOUT_SECONDS);
        let metadata_timeout = Duration::from_secs(METADATA_TIMEOUT_SECONDS);
        let (roster, catalog) = tokio::join!(
            fetch_json(
                client,
                &roster_url,
                Some(&upstream_key),
                None,
                roster_timeout
            ),
            fetch_json(
                client,
                &cfg.registry_metadata_url,
                None,
                previous.and_then(|entry| entry.catalog.etag.as_deref()),
                metadata_timeout,
            ),
        );

        let roster_value = roster?
            .ok_or_else(|| RegistryError::message("upstream /models answered 304"))?
            .parse()?;
        let roster = model_roster(&roster_value)?;

        let (catalog, not_modified) = match catalog? {
            Some(catalog) => (catalog, false),
            None => {
                let previous = previous.ok_or_else(|| {
                    RegistryError::message("metadata answered 304 without a cached copy")
                })?;
                // The catalog did not move: reuse the built registry unless the
                // entitlement roster itself changed.
                if previous.roster == roster {
                    return Ok(Refreshed {
                        data: previous.data.clone(),
                        catalog: previous.catalog.clone(),
                        roster,
                        skipped: Vec::new(),
                        not_modified: true,
                    });
                }
                (previous.catalog.clone(), true)
            }
        };

        let catalog_value = catalog.parse()?;
        let (registry, skipped) = build_registry(
            &roster_value,
            &catalog_value,
            &self.public_base_url,
            &cfg.api_key_env_name,
            self.protocols.as_ref(),
        )?;
        Ok(Refreshed {
            data: registry,
            catalog,
            roster,
            skipped,
            not_modified,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BASE: &str = "http://127.0.0.1:8787";
    const ENV: &str = "OPENCODE_BRIDGE_API_KEY";

    fn roster(ids: &[&str]) -> Value {
        json!({
            "data": ids.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>(),
        })
    }

    fn catalog(models: Value) -> Value {
        json!({
            "opencode-go": { "npm": "@ai-sdk/openai-compatible", "models": models },
        })
    }

    fn build(
        ids: &[&str],
        models: Value,
        protocols: Option<&BTreeSet<Wire>>,
    ) -> (Value, Vec<String>) {
        build_registry(&roster(ids), &catalog(models), BASE, ENV, protocols)
            .expect("registry builds")
    }

    #[test]
    fn buckets_models_by_protocol_with_the_exact_json_shape() {
        let models = json!({
            "chat-model": {
                "name": "Chat",
                "limit": { "context": 1000, "output": 100 },
                "provider": { "npm": "@ai-sdk/openai-compatible" },
                "reasoning": true,
                "tool_call": true,
            },
            "resp-model": {
                "limit": { "context": 2000 },
                "provider": { "npm": "@ai-sdk/openai" },
            },
            "anth-model": {
                "limit": { "context": 3000 },
                "provider": { "npm": "@ai-sdk/anthropic" },
            },
        });
        let (registry, skipped) = build(&["chat-model", "resp-model", "anth-model"], models, None);
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(
            serde_json::to_string(&registry).expect("serialize"),
            r#"{"opencode-go-chat":{"id":"opencode-go-chat","name":"OpenCode Go (Chat)","type":"openai","api":"http://127.0.0.1:8787/zen/go/v1","env":["OPENCODE_BRIDGE_API_KEY"],"models":{"chat-model":{"id":"chat-model","name":"Chat","limit":{"context":1000,"output":100},"reasoning":true,"tool_call":true}}},"opencode-go-responses":{"id":"opencode-go-responses","name":"OpenCode Go (Responses)","type":"openai_responses","api":"http://127.0.0.1:8787/zen/go/v1","env":["OPENCODE_BRIDGE_API_KEY"],"models":{"resp-model":{"id":"resp-model","name":"resp-model","limit":{"context":2000}}}},"opencode-go-anthropic":{"id":"opencode-go-anthropic","name":"OpenCode Go (Anthropic)","type":"anthropic","api":"http://127.0.0.1:8787/zen/go/v1","env":["OPENCODE_BRIDGE_API_KEY"],"models":{"anth-model":{"id":"anth-model","name":"anth-model","limit":{"context":3000}}}}}"#
        );
    }

    #[test]
    fn emits_every_optional_field_in_the_documented_order() {
        let models = json!({
            "m": {
                "name": "M",
                "limit": { "context": 1, "input": 2, "output": 3 },
                "reasoning": false,
                "tool_call": true,
                "modalities": { "input": ["text"] },
                "reasoning_options": [{ "type": "effort", "values": ["low", "high"] }],
                "interleaved": { "reasoning": true },
                "provider": { "npm": "@ai-sdk/anthropic" },
            },
        });
        let (registry, _) = build(&["m"], models, None);
        let dest = &registry["opencode-go-anthropic"]["models"]["m"];
        assert_eq!(
            serde_json::to_string(dest).expect("serialize"),
            r#"{"id":"m","name":"M","limit":{"context":1,"input":2,"output":3},"reasoning":false,"tool_call":true,"modalities":{"input":["text"]},"reasoning_options":[{"type":"effort","values":["low","high"]}],"support_efforts":["low","high"],"interleaved":{"reasoning":true}}"#
        );
    }

    #[test]
    fn falls_back_to_the_entry_level_npm() {
        // No provider override: the opencode-go entry's npm decides the bucket.
        let models = json!({ "m": { "limit": { "context": 10 } } });
        let (registry, skipped) = build(&["m"], models, None);
        assert!(skipped.is_empty(), "{skipped:?}");
        assert!(registry["opencode-go-chat"]["models"]["m"].is_object());
    }

    #[test]
    fn excludes_deprecated_and_alpha_models_with_a_summary() {
        let models = json!({
            "good": { "limit": { "context": 10 } },
            "a1": { "status": "alpha", "limit": { "context": 10 } },
            "a2": { "status": "Alpha", "limit": { "context": 10 } },
            "d1": { "status": "deprecated", "limit": { "context": 10 } },
        });
        let (registry, skipped) = build(&["good", "a1", "a2", "d1"], models, None);
        assert!(registry["opencode-go-chat"]["models"]["good"].is_object());
        assert_eq!(
            skipped,
            vec![
                "status filter excluded 2 alpha model(s): a1, a2".to_string(),
                "status filter excluded 1 deprecated model(s): d1".to_string(),
            ]
        );
    }

    #[test]
    fn truncates_the_status_filter_sample_to_five_ids() {
        let mut models = serde_json::Map::new();
        for index in 0..7 {
            models.insert(
                format!("d{index}"),
                json!({ "status": "deprecated", "limit": { "context": 10 } }),
            );
        }
        models.insert("good".to_string(), json!({ "limit": { "context": 10 } }));
        let (_, skipped) = build(
            &["d0", "d1", "d2", "d3", "d4", "d5", "d6", "good"],
            Value::Object(models),
            None,
        );
        assert_eq!(
            skipped,
            vec![
                "status filter excluded 7 deprecated model(s): d0, d1, d2, d3, d4 (+2 more)"
                    .to_string()
            ]
        );
    }

    #[test]
    fn skips_models_with_missing_metadata_context_or_protocol() {
        let models = json!({
            "good": { "limit": { "context": 10 } },
            "nocontext": { "limit": { "output": 10 } },
            "nullcontext": { "limit": { "context": null } },
            "boolcontext": { "limit": { "context": true } },
            "zerocontext": { "limit": { "context": 0 } },
            "negcontext": { "limit": { "context": -1 } },
            "strcontext": { "limit": { "context": "10" } },
            "floatcontext": { "limit": { "context": 10.5 } },
            "weird": { "limit": { "context": 10 }, "provider": { "npm": "@ai-sdk/other" } },
        });
        let (registry, skipped) = build(
            &[
                "good",
                "ghost",
                "nocontext",
                "nullcontext",
                "boolcontext",
                "zerocontext",
                "negcontext",
                "strcontext",
                "floatcontext",
                "weird",
            ],
            models,
            None,
        );
        assert_eq!(
            registry["opencode-go-chat"]["models"]
                .as_object()
                .expect("models")
                .len(),
            1
        );
        assert_eq!(
            skipped,
            vec![
                "boolcontext: missing context limit".to_string(),
                "floatcontext: missing context limit".to_string(),
                "ghost: no metadata".to_string(),
                "negcontext: missing context limit".to_string(),
                "nocontext: missing context limit".to_string(),
                "nullcontext: missing context limit".to_string(),
                "strcontext: missing context limit".to_string(),
                "weird: unsupported npm protocol Some(\"@ai-sdk/other\")".to_string(),
                "zerocontext: missing context limit".to_string(),
            ]
        );
    }

    #[test]
    fn protocol_filter_keeps_only_the_selected_wire() {
        let models = json!({
            "chat": { "limit": { "context": 10 }, "provider": { "npm": "@ai-sdk/openai-compatible" } },
            "resp": { "limit": { "context": 10 }, "provider": { "npm": "@ai-sdk/openai" } },
            "anth": { "limit": { "context": 10 }, "provider": { "npm": "@ai-sdk/anthropic" } },
        });
        let selected = parse_protocol_selection("chat").expect("selection");
        let (registry, skipped) = build(&["chat", "resp", "anth"], models, selected.as_ref());
        let providers: Vec<&String> = registry.as_object().expect("providers").keys().collect();
        assert_eq!(providers, vec!["opencode-go-chat"]);
        assert_eq!(
            skipped,
            vec![
                "protocol filter excluded 1 anthropic model(s)".to_string(),
                "protocol filter excluded 1 openai_responses model(s)".to_string(),
            ]
        );
    }

    #[test]
    fn errors_when_nothing_is_importable() {
        let models = json!({ "m": { "status": "deprecated", "limit": { "context": 10 } } });
        let error = build_registry(&roster(&["m"]), &catalog(models), BASE, ENV, None)
            .expect_err("nothing importable");
        assert_eq!(
            error.to_string(),
            "no importable models for the selected protocols (anthropic, openai, openai_responses); check Go entitlement and metadata"
        );
    }

    #[test]
    fn errors_when_the_catalog_lacks_the_opencode_go_entry() {
        for catalog in [
            json!({}),
            json!({ "opencode-go": {} }),
            json!({ "opencode-go": { "models": "nope" } }),
            json!({ "opencode-go": "nope" }),
        ] {
            let error = build_registry(&roster(&["m"]), &catalog, BASE, ENV, None)
                .expect_err("broken catalog");
            assert_eq!(error.to_string(), "catalog missing opencode-go.models");
        }
    }

    #[test]
    fn roster_requires_an_array_of_safe_model_ids() {
        assert_eq!(
            model_roster(&json!({})).expect_err("no data").to_string(),
            "unexpected upstream /models response"
        );
        assert_eq!(
            model_roster(&json!({ "data": {} }))
                .expect_err("data not a list")
                .to_string(),
            "unexpected upstream /models response"
        );
        assert_eq!(
            model_roster(&json!({ "data": [] }))
                .expect_err("empty")
                .to_string(),
            "upstream /models returned no valid model IDs"
        );
        assert_eq!(
            model_roster(&json!({
                "data": [
                    { "id": "ok.ok" },
                    { "id": "bad id" },
                    { "id": "-leading" },
                    { "id": "" },
                    { "id": 7 },
                    {},
                ],
            }))
            .expect("safe ids only"),
            BTreeSet::from(["ok.ok".to_string()])
        );
    }

    #[test]
    fn effort_values_accepts_both_documented_shapes() {
        let object_form = json!({
            "reasoning_options": [{ "type": "effort", "values": ["low", "medium"] }],
        });
        assert_eq!(
            effort_values(object_form.as_object().expect("object")),
            Some(vec!["low".to_string(), "medium".to_string()])
        );

        let legacy_form = json!({ "reasoning_options": ["low", "high"] });
        assert_eq!(
            effort_values(legacy_form.as_object().expect("object")),
            Some(vec!["low".to_string(), "high".to_string()])
        );

        for rejected in [
            json!({}),
            json!({ "reasoning_options": [] }),
            json!({ "reasoning_options": "low" }),
            json!({ "reasoning_options": [1, 2] }),
            json!({ "reasoning_options": ["low", ""] }),
            json!({ "reasoning_options": [["low"]] }),
            json!({ "reasoning_options": [{ "type": "effort", "values": [] }] }),
            json!({ "reasoning_options": [{ "type": "effort", "values": ["low", 2] }] }),
            json!({ "reasoning_options": [{ "type": "other", "values": ["low"] }] }),
        ] {
            assert_eq!(
                effort_values(rejected.as_object().expect("object")),
                None,
                "{rejected}"
            );
        }
    }

    #[test]
    fn protocol_selection_accepts_aliases_and_rejects_unknown_tokens() {
        assert_eq!(parse_protocol_selection("all"), Ok(None));
        assert_eq!(
            parse_protocol_selection(" chat , openai "),
            Ok(Some(BTreeSet::from([Wire::OpenAi])))
        );
        assert_eq!(
            parse_protocol_selection("responses"),
            Ok(Some(BTreeSet::from([Wire::OpenAiResponses])))
        );
        assert_eq!(
            parse_protocol_selection("openai_responses,anthropic"),
            Ok(Some(BTreeSet::from([
                Wire::OpenAiResponses,
                Wire::Anthropic
            ])))
        );
        assert_eq!(
            parse_protocol_selection(""),
            Err("no protocols given".to_string())
        );
        assert_eq!(
            parse_protocol_selection(" , "),
            Err("no protocols given".to_string())
        );
        assert_eq!(
            parse_protocol_selection("bogus"),
            Err(
                "unknown protocol \"bogus\"; expected chat, responses, anthropic, or all"
                    .to_string()
            )
        );
    }

    #[test]
    fn model_ids_are_sorted_by_the_roster_iteration() {
        let models = json!({
            "b": { "limit": { "context": 10 } },
            "a": { "limit": { "context": 10 } },
            "c": { "limit": { "context": 10 } },
        });
        let (registry, _) = build(&["b", "c", "a"], models, None);
        let ids: Vec<&String> = registry["opencode-go-chat"]["models"]
            .as_object()
            .expect("models")
            .keys()
            .collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn revalidates_metadata_with_the_cached_etag() {
        use axum::http::{header, HeaderMap, HeaderValue, StatusCode as HttpStatus};
        use axum::routing::get;

        async fn catalog(headers: HeaderMap) -> (HttpStatus, HeaderMap, &'static str) {
            let mut response_headers = HeaderMap::new();
            response_headers.insert(header::ETAG, HeaderValue::from_static("\"v1\""));
            // Both compression features must be negotiated: the catalog is
            // ~14x smaller over the wire with brotli.
            let negotiated = headers
                .get(header::ACCEPT_ENCODING)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.contains("br") && value.contains("gzip"));
            if !negotiated {
                return (HttpStatus::NOT_ACCEPTABLE, response_headers, "");
            }
            let unchanged = headers
                .get(header::IF_NONE_MATCH)
                .and_then(|value| value.to_str().ok())
                == Some("\"v1\"");
            if unchanged {
                return (HttpStatus::NOT_MODIFIED, response_headers, "");
            }
            (
                HttpStatus::OK,
                response_headers,
                r#"{"opencode-go":{"models":{}}}"#,
            )
        }

        let app = axum::Router::new().route("/catalog.json", get(catalog));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/catalog.json");
        let timeout = Duration::from_secs(5);

        let fresh = fetch_json(&client, &url, None, None, timeout)
            .await
            .expect("first fetch")
            .expect("200 body");
        assert_eq!(fresh.etag.as_deref(), Some("\"v1\""));
        assert!(fresh.parse().expect("json")["opencode-go"].is_object());

        let revalidated = fetch_json(&client, &url, None, fresh.etag.as_deref(), timeout)
            .await
            .expect("second fetch");
        assert!(revalidated.is_none(), "304 should reuse the cached body");
    }
}
