//! Validated bridge configuration (YAML) plus its live store.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_norway::Value as YamlValue;

use crate::keygen::generate_api_key;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("config {path} is not valid YAML: {source}")]
    Yaml {
        path: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("config root in {path} must be a mapping")]
    Root { path: String },
    #[error("invalid config {path}: {message}")]
    Invalid { path: String, message: String },
    #[error("cannot write config {path}: {source}")]
    Write {
        path: String,
        source: std::io::Error,
    },
}

fn d_listen() -> String {
    "127.0.0.1:8787".to_string()
}

fn d_upstream_base_url() -> String {
    "https://opencode.ai".to_string()
}

fn d_upstream_api_prefix() -> String {
    "/inference/go/v1".to_string()
}

fn d_api_key_env_name() -> String {
    "OPENCODE_BRIDGE_API_KEY".to_string()
}

fn d_registry_metadata_url() -> String {
    "https://models.opencode.ai/api.json".to_string()
}

fn d_registry_ttl_seconds() -> u64 {
    900
}

fn d_max_body_bytes() -> usize {
    80 * 1024 * 1024
}

fn d_upstream_timeout_seconds() -> f64 {
    300.0
}

/// Validated bridge configuration. Field names and defaults match the YAML
/// template in `config.example.yaml` byte-for-byte.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeConfig {
    #[serde(default = "d_listen")]
    pub listen: String,
    #[serde(default = "d_upstream_base_url")]
    pub upstream_base_url: String,
    #[serde(default = "d_upstream_api_prefix")]
    pub upstream_api_prefix: String,
    #[serde(default)]
    pub upstream_api_key: String,
    /// Any key in this list authenticates a client against the bridge.
    #[serde(default)]
    pub proxy_api_keys: Vec<String>,
    #[serde(default = "d_api_key_env_name")]
    pub api_key_env_name: String,
    #[serde(default = "d_registry_metadata_url")]
    pub registry_metadata_url: String,
    #[serde(default = "d_registry_ttl_seconds")]
    pub registry_ttl_seconds: u64,
    #[serde(default = "d_max_body_bytes")]
    pub max_body_bytes: usize,
    #[serde(default = "d_upstream_timeout_seconds")]
    pub upstream_timeout_seconds: f64,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            listen: d_listen(),
            upstream_base_url: d_upstream_base_url(),
            upstream_api_prefix: d_upstream_api_prefix(),
            upstream_api_key: String::new(),
            proxy_api_keys: Vec::new(),
            api_key_env_name: d_api_key_env_name(),
            registry_metadata_url: d_registry_metadata_url(),
            registry_ttl_seconds: d_registry_ttl_seconds(),
            max_body_bytes: d_max_body_bytes(),
            upstream_timeout_seconds: d_upstream_timeout_seconds(),
        }
    }
}

/// Name of the environment variable that overrides `upstream_api_key`.
pub const UPSTREAM_KEY_ENV: &str = "OPENCODE_BRIDGE_UPSTREAM_API_KEY";

/// Default config location when neither `-c` nor `OPENCODE_BRIDGE_CONFIG` is
/// given: `~/.config/ocbk/config.yaml`. Falls back to the historical
/// `config.yaml` in the working directory when `$HOME` is unset.
pub fn default_config_path() -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => format!("{home}/.config/ocbk/config.yaml"),
        _ => "config.yaml".to_string(),
    }
}

impl BridgeConfig {
    /// Apply the same normalization the Python validators perform.
    fn normalize(mut self) -> Result<Self, String> {
        parse_listen(&self.listen)?;

        self.upstream_base_url = check_absolute_http("upstream_base_url", &self.upstream_base_url)?;
        self.upstream_api_prefix = check_prefix(&self.upstream_api_prefix)?;

        self.upstream_api_key = self.upstream_api_key.trim().to_string();
        self.api_key_env_name = self.api_key_env_name.trim().to_string();
        if self.api_key_env_name.is_empty() {
            return Err("api_key_env_name must not be empty".to_string());
        }

        let keys = self
            .proxy_api_keys
            .iter()
            .map(|key| key.trim())
            .filter(|key| !key.is_empty())
            .fold(Vec::new(), |mut acc, key| {
                if !acc.iter().any(|seen: &String| seen == key) {
                    acc.push(key.to_string());
                }
                acc
            });
        self.proxy_api_keys = keys;

        if self.registry_ttl_seconds < 1 {
            return Err("registry_ttl_seconds must be >= 1".to_string());
        }
        if self.max_body_bytes < 1 {
            return Err("max_body_bytes must be >= 1".to_string());
        }
        if !(self.upstream_timeout_seconds.is_finite() && self.upstream_timeout_seconds >= 1.0) {
            return Err("upstream_timeout_seconds must be >= 1".to_string());
        }

        Ok(self)
    }

    /// The upstream key actually used, honouring the environment override.
    pub fn effective_upstream_api_key(&self) -> String {
        match std::env::var(UPSTREAM_KEY_ENV) {
            Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
            _ => self.upstream_api_key.clone(),
        }
    }

    pub fn listen_addr(&self) -> (String, u16) {
        parse_listen(&self.listen).expect("listen was validated when the config was loaded")
    }

    pub fn upstream_prefix(&self) -> &str {
        if self.upstream_api_prefix.is_empty() {
            "/"
        } else {
            &self.upstream_api_prefix
        }
    }
}

/// The URL the bridge publishes to clients: the host and port of `listen`,
/// which is the socket they actually reach.
///
/// A wildcard `listen` (0.0.0.0 / [::]) names no connectable address, so it
/// publishes loopback. `--host` overrides the host, or the whole URL, for
/// what `listen` cannot express: a wildcard bind reached from another
/// machine, or a TLS-terminating proxy in front.
pub fn public_base_url(listen: &str, host: Option<&str>) -> Result<String, String> {
    let (listen_host, port) = parse_listen(listen)?;
    let Some(host) = host.map(str::trim).filter(|host| !host.is_empty()) else {
        let host = if is_wildcard_host(&listen_host) {
            "127.0.0.1".to_string()
        } else {
            url_host(&listen_host)
        };
        return Ok(format!("http://{host}:{port}"));
    };

    if host.contains("://") {
        return check_absolute_http("--host", host);
    }
    if host.contains(char::is_whitespace) || host.contains('/') {
        return Err(format!(
            "--host must be a host, host:port or an absolute http(s) URL, got {host:?}"
        ));
    }
    if host_has_port(host) {
        return check_absolute_http("--host", &format!("http://{host}"));
    }
    check_absolute_http("--host", &format!("http://{}", url_host(host)))?;
    Ok(format!("http://{}:{port}", url_host(host)))
}

fn is_wildcard_host(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "::" | "*")
}

/// Re-add the brackets `parse_listen` strips from an IPv6 host, so it can go
/// back into a URL.
fn url_host(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// A `--host` value carries a port when a colon appears outside brackets.
fn host_has_port(host: &str) -> bool {
    match host.strip_prefix('[') {
        Some(rest) => rest
            .split_once(']')
            .is_some_and(|(_, tail)| tail.starts_with(':')),
        None => host.contains(':'),
    }
}

fn check_absolute_http(field: &str, value: &str) -> Result<String, String> {
    let split = urlsplit(value);
    if !matches!(split.scheme, "http" | "https") || split.hostname().is_none() {
        return Err(format!(
            "{field} must be an absolute http(s) URL, got {value:?}"
        ));
    }
    Ok(value.trim_end_matches('/').to_string())
}

fn check_prefix(value: &str) -> Result<String, String> {
    let value = value.trim();
    if !value.starts_with('/') {
        return Err("upstream_api_prefix must start with '/'".to_string());
    }
    let split = urlsplit(value);
    if !split.scheme.is_empty() || !split.netloc.is_empty() {
        return Err("upstream_api_prefix must be a path, not a full URL".to_string());
    }
    let trimmed = value.trim_end_matches('/');
    Ok(if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    })
}

/// Split `host:port`, accepting bracketed IPv6 hosts as Python's
/// `str.rpartition` based parser does.
pub fn parse_listen(listen: &str) -> Result<(String, u16), String> {
    let invalid = || format!("listen must look like host:port, got {listen:?}");
    let (host, port) = listen.rsplit_once(':').ok_or_else(invalid)?;
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let port: u16 = port.parse().map_err(|_| invalid())?;
    let host = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    Ok((
        if host.is_empty() {
            "127.0.0.1".to_string()
        } else {
            host.to_string()
        },
        port,
    ))
}

struct SplitUrl<'a> {
    scheme: &'a str,
    netloc: &'a str,
}

impl SplitUrl<'_> {
    fn hostname(&self) -> Option<&str> {
        let authority = self.netloc.rsplit('@').next()?;
        if let Some(rest) = authority.strip_prefix('[') {
            let host = rest.split(']').next()?;
            return (!host.is_empty()).then_some(host);
        }
        let host = authority.split(':').next()?;
        (!host.is_empty()).then_some(host)
    }
}

/// The subset of `urllib.parse.urlsplit` this crate relies on: a scheme is
/// only recognised before any of `/`, `?` or `#`, and a netloc only follows a
/// leading `//`.
fn urlsplit(value: &str) -> SplitUrl<'_> {
    let mut rest = value;
    let mut scheme = "";
    if let Some(index) = rest.find(':') {
        let candidate = &rest[..index];
        let looks_like_scheme = !candidate.is_empty()
            && candidate.as_bytes()[0].is_ascii_alphabetic()
            && candidate
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'));
        if looks_like_scheme {
            scheme = candidate;
            rest = &rest[index + 1..];
        }
    }
    let netloc = match rest.strip_prefix("//") {
        Some(after) => {
            let end = after.find(['/', '?', '#']).unwrap_or(after.len());
            &after[..end]
        }
        None => "",
    };
    SplitUrl { scheme, netloc }
}

/// Holds the live config; swapped in place on SIGHUP reload.
#[derive(Clone)]
pub struct ConfigStore {
    inner: Arc<RwLock<Arc<BridgeConfig>>>,
}

impl ConfigStore {
    pub fn new(config: BridgeConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Arc::new(config))),
        }
    }

    pub fn get(&self) -> Arc<BridgeConfig> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn replace(&self, config: BridgeConfig) {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Arc::new(config);
    }
}

pub fn write_config(path: &str, config: &BridgeConfig) -> Result<(), ConfigError> {
    let yaml = serde_norway::to_string(config).map_err(|error| ConfigError::Write {
        path: path.to_string(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()),
    })?;

    let parent = Path::new(path)
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty());
    if let Some(dir) = parent {
        std::fs::create_dir_all(dir).map_err(|source| ConfigError::Write {
            path: dir.to_string_lossy().into_owned(),
            source,
        })?;
    }

    let tmp = format!("{path}.tmp");
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).map_err(|source| ConfigError::Write {
        path: tmp.clone(),
        source,
    })?;
    file.write_all(yaml.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|source| ConfigError::Write {
            path: tmp.clone(),
            source,
        })?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(|source| ConfigError::Write {
        path: path.to_string(),
        source,
    })
}

pub fn load_config(path: &str) -> Result<BridgeConfig, ConfigError> {
    let text = std::fs::read(path).map_err(|source| ConfigError::Read {
        path: path.to_string(),
        source,
    })?;
    let text = String::from_utf8(text).map_err(|error| ConfigError::Yaml {
        path: path.to_string(),
        source: Box::new(error),
    })?;

    let document: YamlValue = serde_norway::from_str(&text).map_err(|error| ConfigError::Yaml {
        path: path.to_string(),
        source: Box::new(error),
    })?;

    let mut mapping = match document {
        YamlValue::Null => serde_norway::Mapping::new(),
        YamlValue::Mapping(mapping) => mapping,
        _ => {
            return Err(ConfigError::Root {
                path: path.to_string(),
            })
        }
    };

    // Legacy single-key field, migrated only when the list is absent.
    if let Some(legacy) = mapping.remove("proxy_api_key") {
        let absent = !mapping.contains_key("proxy_api_keys");
        if absent {
            if let YamlValue::String(legacy) = legacy {
                if !legacy.trim().is_empty() {
                    mapping.insert(
                        YamlValue::String("proxy_api_keys".to_string()),
                        YamlValue::Sequence(vec![YamlValue::String(legacy)]),
                    );
                }
            }
        }
    }

    let config: BridgeConfig =
        serde_norway::from_value(YamlValue::Mapping(mapping)).map_err(|error| {
            ConfigError::Invalid {
                path: path.to_string(),
                message: error.to_string(),
            }
        })?;

    config.normalize().map_err(|message| ConfigError::Invalid {
        path: path.to_string(),
        message,
    })
}

pub enum Bootstrap {
    /// The config already existed (or was repaired in place); run normally.
    Ready(BridgeConfig),
    /// A fresh config was written; the caller reports it and exits with code 2.
    Created(BridgeConfig),
}

pub fn load_or_bootstrap(path: &str) -> Result<Bootstrap, ConfigError> {
    if !Path::new(path).exists() {
        let config = BridgeConfig {
            proxy_api_keys: vec![generate_api_key()],
            ..BridgeConfig::default()
        };
        write_config(path, &config)?;
        return Ok(Bootstrap::Created(config));
    }

    let mut config = load_config(path)?;
    if config.proxy_api_keys.is_empty() {
        config.proxy_api_keys = vec![generate_api_key()];
        write_config(path, &config)?;
    }
    Ok(Bootstrap::Ready(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn parse(yaml: &str) -> Result<BridgeConfig, ConfigError> {
        let path = temp_path("parse");
        std::fs::write(&path, yaml).expect("write temp config");
        let result = load_config(&path);
        let _ = std::fs::remove_file(&path);
        result
    }

    fn temp_path(tag: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "opencode-bridge-test-{}-{tag}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("config.yaml").to_string_lossy().into_owned()
    }

    #[test]
    fn defaults_match_the_documented_template() {
        let config = parse("{}").expect("defaults parse");
        assert_eq!(config.listen, "127.0.0.1:8787");
        assert_eq!(config.upstream_base_url, "https://opencode.ai");
        assert_eq!(config.upstream_api_prefix, "/inference/go/v1");
        assert_eq!(config.upstream_api_key, "");
        assert!(config.proxy_api_keys.is_empty());
        assert_eq!(config.api_key_env_name, "OPENCODE_BRIDGE_API_KEY");
        assert_eq!(
            config.registry_metadata_url,
            "https://models.opencode.ai/api.json"
        );
        assert_eq!(config.registry_ttl_seconds, 900);
        assert_eq!(config.max_body_bytes, 83_886_080);
        assert_eq!(config.upstream_timeout_seconds, 300.0);
    }

    #[test]
    fn empty_document_is_defaults() {
        let config = parse("").expect("empty document");
        assert_eq!(config, BridgeConfig::default());
    }

    #[test]
    fn rejects_unknown_fields() {
        let error = parse("listen: 127.0.0.1:1\nnope: 1\n").expect_err("unknown field");
        assert!(
            matches!(error, ConfigError::Invalid { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn rejects_non_mapping_root() {
        let error = parse("- 1\n").expect_err("sequence root");
        assert!(matches!(error, ConfigError::Root { .. }), "got {error:?}");
    }

    #[test]
    fn rejects_invalid_listen() {
        for listen in ["8787", "host:port", "host:", ":0abc"] {
            let error = parse(&format!("listen: \"{listen}\"\n")).expect_err(listen);
            assert!(
                matches!(error, ConfigError::Invalid { .. }),
                "{listen} -> {error:?}"
            );
        }
    }

    #[test]
    fn accepts_bracketed_ipv6_listen() {
        let config = parse("listen: \"[::1]:8787\"\n").expect("ipv6 listen");
        assert_eq!(config.listen_addr(), ("::1".to_string(), 8787));
    }

    #[test]
    fn empty_host_defaults_to_loopback() {
        assert_eq!(
            parse_listen(":9000").expect("port only"),
            ("127.0.0.1".to_string(), 9000)
        );
    }

    #[test]
    fn derives_the_published_url_from_listen() {
        for (listen, expected) in [
            ("127.0.0.1:8787", "http://127.0.0.1:8787"),
            ("localhost:9000", "http://localhost:9000"),
            ("0.0.0.0:8787", "http://127.0.0.1:8787"),
            ("[::]:8787", "http://127.0.0.1:8787"),
            ("[::1]:9000", "http://[::1]:9000"),
        ] {
            assert_eq!(
                public_base_url(listen, None).expect(listen),
                expected,
                "{listen}"
            );
        }
    }

    #[test]
    fn host_override_replaces_the_host_or_the_whole_url() {
        assert_eq!(
            public_base_url("0.0.0.0:8787", Some("192.168.1.10")).expect("host"),
            "http://192.168.1.10:8787"
        );
        assert_eq!(
            public_base_url("0.0.0.0:8787", Some("192.168.1.10:9000")).expect("host:port"),
            "http://192.168.1.10:9000"
        );
        assert_eq!(
            public_base_url("127.0.0.1:8787", Some("[::1]")).expect("ipv6 host"),
            "http://[::1]:8787"
        );
        assert_eq!(
            public_base_url("127.0.0.1:8787", Some("https://bridge.example.com/")).expect("url"),
            "https://bridge.example.com"
        );
        assert_eq!(
            public_base_url("127.0.0.1:8787", Some("  ")).expect("blank means absent"),
            "http://127.0.0.1:8787"
        );
        for bad in ["ftp://example.com", "bad host", "host/path"] {
            assert!(
                public_base_url("127.0.0.1:8787", Some(bad)).is_err(),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_relative_and_non_http_urls() {
        let yaml = "upstream_base_url: \"http://\"\n";
        let error = parse(yaml).expect_err(yaml);
        assert!(
            matches!(error, ConfigError::Invalid { .. }),
            "{yaml} -> {error:?}"
        );
    }

    #[test]
    fn strips_trailing_slash_from_base_urls() {
        let config = parse("upstream_base_url: https://opencode.ai///\n").expect("trailing slash");
        assert_eq!(config.upstream_base_url, "https://opencode.ai");
    }

    #[test]
    fn rejects_prefix_that_is_not_a_path() {
        for prefix in [
            "inference/go/v1",
            "https://opencode.ai/inference",
            "//host/v1",
        ] {
            let error = parse(&format!("upstream_api_prefix: \"{prefix}\"\n")).expect_err(prefix);
            assert!(
                matches!(error, ConfigError::Invalid { .. }),
                "{prefix} -> {error:?}"
            );
        }
    }

    #[test]
    fn normalizes_prefix() {
        let config = parse("upstream_api_prefix: /zen/go/v1/\n").expect("prefix");
        assert_eq!(config.upstream_api_prefix, "/zen/go/v1");
        let root = parse("upstream_api_prefix: /\n").expect("root prefix");
        assert_eq!(root.upstream_api_prefix, "/");
        assert_eq!(root.upstream_prefix(), "/");
    }

    #[test]
    fn trims_and_dedupes_proxy_api_keys_in_order() {
        let config = parse("proxy_api_keys:\n  - \" a \"\n  - b\n  - a\n  - \"\"\n  - \"  \"\n")
            .expect("keys");
        assert_eq!(
            config.proxy_api_keys,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn migrates_legacy_single_key() {
        let config = parse("proxy_api_key: legacy-key\n").expect("legacy key");
        assert_eq!(config.proxy_api_keys, vec!["legacy-key".to_string()]);

        // An explicit list wins, and the legacy key is dropped either way.
        let both =
            parse("proxy_api_key: legacy\nproxy_api_keys:\n  - listed\n").expect("both keys");
        assert_eq!(both.proxy_api_keys, vec!["listed".to_string()]);

        // Blank legacy values are ignored, matching the Python validator.
        let blank = parse("proxy_api_key: \"   \"\n").expect("blank legacy");
        assert!(blank.proxy_api_keys.is_empty());
    }

    #[test]
    fn rejects_empty_api_key_env_name() {
        let error = parse("api_key_env_name: \"  \"\n").expect_err("blank env name");
        assert!(
            matches!(error, ConfigError::Invalid { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn rejects_out_of_range_numbers() {
        for yaml in [
            "registry_ttl_seconds: 0\n",
            "max_body_bytes: 0\n",
            "upstream_timeout_seconds: 0.5\n",
        ] {
            let error = parse(yaml).expect_err(yaml);
            assert!(
                matches!(error, ConfigError::Invalid { .. }),
                "{yaml} -> {error:?}"
            );
        }
    }

    #[test]
    fn missing_file_is_a_read_error() {
        let error = load_config("/nonexistent/definitely/not/here.yaml").expect_err("missing");
        assert!(matches!(error, ConfigError::Read { .. }), "got {error:?}");
    }

    #[test]
    fn malformed_yaml_is_a_yaml_error() {
        let error = parse("listen: [unclosed\n").expect_err("malformed");
        assert!(matches!(error, ConfigError::Yaml { .. }), "got {error:?}");
    }

    #[test]
    fn bootstrap_creates_a_config_with_one_key() {
        let path = temp_path("bootstrap");
        let _ = std::fs::remove_file(&path);
        let outcome = load_or_bootstrap(&path).expect("bootstrap");
        let Bootstrap::Created(config) = outcome else {
            panic!("expected a fresh config");
        };
        assert_eq!(config.proxy_api_keys.len(), 1);
        let key = &config.proxy_api_keys[0];
        assert!(key.starts_with("sk-bridge-"), "{key}");
        assert_eq!(key.len(), "sk-bridge-".len() + 43, "{key}");

        // The written file loads back unchanged and is not world readable.
        let reloaded = load_config(&path).expect("reload");
        assert_eq!(reloaded.proxy_api_keys, config.proxy_api_keys);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");
        }
        assert!(!std::path::Path::new(&format!("{path}.tmp")).exists());
    }

    #[test]
    fn bootstrap_fills_in_a_missing_key_list() {
        let path = temp_path("repair");
        std::fs::write(&path, "listen: 127.0.0.1:9999\n").expect("seed");
        let outcome = load_or_bootstrap(&path).expect("repair");
        let Bootstrap::Ready(config) = outcome else {
            panic!("expected the existing config to be reused");
        };
        assert_eq!(config.listen, "127.0.0.1:9999");
        assert_eq!(config.proxy_api_keys.len(), 1);
        let reloaded = load_config(&path).expect("reload");
        assert_eq!(reloaded.proxy_api_keys, config.proxy_api_keys);
    }

    #[test]
    fn bootstrap_creates_missing_parent_directories() {
        let base = temp_path("parents");
        let path = format!("{base}/nested/ocbk/config.yaml");
        let outcome = load_or_bootstrap(&path).expect("bootstrap into missing dirs");
        assert!(matches!(outcome, Bootstrap::Created(_)));
        assert!(load_config(&path).is_ok());
    }

    #[test]
    fn store_swaps_the_config_in_place() {
        let store = ConfigStore::new(BridgeConfig::default());
        assert_eq!(store.get().listen, "127.0.0.1:8787");
        store.replace(BridgeConfig {
            listen: "127.0.0.1:1".to_string(),
            ..BridgeConfig::default()
        });
        assert_eq!(store.get().listen, "127.0.0.1:1");
    }

    #[test]
    fn written_config_round_trips_every_field() {
        let path = temp_path("roundtrip");
        let config = BridgeConfig {
            listen: "127.0.0.1:1234".to_string(),
            upstream_api_key: "secret".to_string(),
            proxy_api_keys: vec!["a".to_string(), "b".to_string()],
            max_body_bytes: 1024,
            upstream_timeout_seconds: 12.5,
            ..BridgeConfig::default()
        };
        write_config(&path, &config).expect("write");
        let reloaded = load_config(&path).expect("reload");
        // `secret` must survive: normalize() only trims.
        assert_eq!(reloaded, config);
    }
}
