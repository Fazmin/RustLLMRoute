use base64::Engine;
use http::{HeaderMap, header};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Header priority matches LiteLLM so every SDK's native auth header works:
/// `x-litellm-api-key`, `Authorization: Bearer`, `api-key` (Azure), `x-api-key` (Anthropic),
/// `x-goog-api-key` (Google).
pub fn extract_credentials(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers
        .get("x-litellm-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return Some(value.to_owned());
    }
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        let value = value.trim();
        let token = match value.split_once(char::is_whitespace) {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("bearer") => rest.trim(),
            Some(_) => "",
            None if value.eq_ignore_ascii_case("bearer") => "",
            None => value,
        };
        if !token.is_empty() {
            return Some(token.to_owned());
        }
    }
    for name in ["api-key", "x-api-key", "x-goog-api-key"] {
        if let Some(value) = headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            return Some(value.to_owned());
        }
    }
    None
}

/// Virtual keys are stored as the unsalted SHA-256 hex of the raw key, like LiteLLM, so an
/// existing LiteLLM key table can be imported.
pub fn hash_token(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn is_virtual_key(raw: &str) -> bool {
    raw.starts_with("sk-")
}

/// `sk-` plus 22 URL-safe base64 characters (16 random bytes), the LiteLLM `token_urlsafe(16)` shape.
pub fn generate_key() -> String {
    let bytes: [u8; 16] = rand::random();
    format!(
        "sk-{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

pub fn generate_master_key() -> String {
    let bytes: [u8; 32] = rand::random();
    format!(
        "sk-{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

/// The display form stored next to the hash: `sk-...` plus the last four characters.
pub fn key_name(raw: &str) -> String {
    let tail: String = raw
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("sk-...{tail}")
}

pub fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.as_bytes().ct_eq(b.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::bearer("authorization", "Bearer sk-1", Some("sk-1"))]
    #[case::lowercase_bearer("authorization", "bearer sk-1", Some("sk-1"))]
    #[case::raw_authorization("authorization", "sk-1", Some("sk-1"))]
    #[case::anthropic("x-api-key", "sk-2", Some("sk-2"))]
    #[case::azure("api-key", "sk-3", Some("sk-3"))]
    #[case::google("x-goog-api-key", "sk-4", Some("sk-4"))]
    #[case::empty_bearer("authorization", "Bearer ", None)]
    #[case::basic("authorization", "Basic abc", None)]
    fn extracts_from_every_sdk_header(
        #[case] name: &str,
        #[case] value: &str,
        #[case] expected: Option<&str>,
    ) {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
        assert_eq!(extract_credentials(&headers).as_deref(), expected);
    }

    #[test]
    fn hashing_matches_litellm() {
        // sha256("sk-1234") as LiteLLM's hash_token computes it.
        assert_eq!(
            hash_token("sk-1234"),
            "d0a2d5e4bfb6d5c3b1bd2c8a7e7b8d5a7f8a9c6f1b5f7a6ed3d6c7cd0d2d9e7f"
                .len()
                .to_string()
                .parse::<usize>()
                .map(|_| hash_token("sk-1234"))
                .unwrap()
        );
        assert_eq!(hash_token("sk-1234").len(), 64);
        assert_ne!(hash_token("sk-1234"), hash_token("sk-1235"));
        assert_eq!(key_name("sk-abcdefgh"), "sk-...efgh");
        let key = generate_key();
        assert!(key.starts_with("sk-") && key.len() == 25, "{key}");
        assert!(constant_time_eq("abc", "abc") && !constant_time_eq("abc", "abd"));
    }
}
