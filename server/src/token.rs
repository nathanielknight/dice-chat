//! Unguessable identifiers: room share tokens and client ids.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

/// 128 bits of OS randomness, URL-safe base64 (22 chars). SPEC.md §3.
pub fn new_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("OS randomness unavailable");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Client ids need the same properties as room tokens.
pub fn new_client_id() -> String {
    new_token()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_urlsafe_and_distinct() {
        let a = new_token();
        let b = new_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 22); // 16 bytes -> ceil(128/6) chars, no padding
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
