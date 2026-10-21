//! Bridge API-key generation.
//!
//! The format matches the Python original exactly: `sk-bridge-` followed by
//! 32 random bytes encoded as unpadded URL-safe base64 (43 characters).

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

pub const DEFAULT_KEY_PREFIX: &str = "sk-bridge-";

pub fn generate_api_key() -> String {
    generate_api_key_with_prefix(DEFAULT_KEY_PREFIX)
}

pub fn generate_api_key_with_prefix(prefix: &str) -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS random source must be available");
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_python_key_shape() {
        let key = generate_api_key();
        assert!(key.starts_with("sk-bridge-"), "{key}");
        assert_eq!(key.len(), "sk-bridge-".len() + 43, "{key}");
        let body = &key["sk-bridge-".len()..];
        assert!(
            body.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
            "{key}"
        );
    }

    #[test]
    fn honours_a_custom_prefix() {
        let key = generate_api_key_with_prefix("sk-team-a-");
        assert!(key.starts_with("sk-team-a-"), "{key}");
    }

    #[test]
    fn keys_are_unique() {
        let first = generate_api_key();
        let second = generate_api_key();
        assert_ne!(first, second);
    }
}
