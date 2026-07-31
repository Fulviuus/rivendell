//! Agent API keys.
//!
//! Format: `rvd_<key_id>_<secret>` where key_id is a public 12-char lookup
//! handle and secret is 32 bytes of CSPRNG output, base64url-encoded.
//!
//! Only `sha256(full_key)` is persisted. Because the secret is full-entropy
//! random there is nothing to brute-force, so a plain digest is sufficient —
//! a password KDF would only slow down every request for no gain.

use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";

pub struct GeneratedKey {
    pub full: String,
    pub key_id: String,
    pub hash: String,
    /// Safe to persist and show in the UI after the one-time reveal.
    pub preview: String,
}

pub fn generate() -> GeneratedKey {
    let mut rng = rand::thread_rng();

    let mut id_bytes = [0u8; 12];
    rng.fill_bytes(&mut id_bytes);
    let key_id: String = id_bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect();

    let mut secret = [0u8; 32];
    rng.fill_bytes(&mut secret);
    let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);

    let full = format!("rvd_{key_id}_{secret}");
    let preview = format!("rvd_{key_id}_…{}", &secret[secret.len() - 4..]);

    GeneratedKey {
        hash: hash(&full),
        full,
        key_id,
        preview,
    }
}

pub fn hash(key: &str) -> String {
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    format!("{:x}", h.finalize())
}

/// Pulls the lookup handle out of a presented token without trusting the rest.
pub fn key_id_of(token: &str) -> Option<&str> {
    let rest = token.strip_prefix("rvd_")?;
    let (id, secret) = rest.split_once('_')?;
    if id.is_empty() || secret.is_empty() {
        return None;
    }
    Some(id)
}

/// Constant-time comparison of two hex digests.
pub fn verify(presented: &str, stored_hash: &str) -> bool {
    let a = hash(presented);
    if a.len() != stored_hash.len() {
        return false;
    }
    a.bytes()
        .zip(stored_hash.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Strips `Bearer ` (case-insensitively) from an Authorization header value.
pub fn strip_bearer(header: &str) -> &str {
    let t = header.trim();
    if t.len() >= 7 && t[..7].eq_ignore_ascii_case("bearer ") {
        t[7..].trim()
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let k = generate();
        assert!(k.full.starts_with("rvd_"));
        assert_eq!(key_id_of(&k.full), Some(k.key_id.as_str()));
        assert!(verify(&k.full, &k.hash));
        assert!(!verify("rvd_x_y", &k.hash));
    }

    #[test]
    fn keys_are_distinct() {
        assert_ne!(generate().full, generate().full);
    }

    #[test]
    fn bearer_parsing() {
        assert_eq!(strip_bearer("Bearer abc"), "abc");
        assert_eq!(strip_bearer("bearer  abc "), "abc");
        assert_eq!(strip_bearer("abc"), "abc");
    }

    #[test]
    fn malformed_tokens_rejected() {
        assert_eq!(key_id_of("nope"), None);
        assert_eq!(key_id_of("rvd_onlyid"), None);
        assert_eq!(key_id_of("rvd__secret"), None);
    }
}
