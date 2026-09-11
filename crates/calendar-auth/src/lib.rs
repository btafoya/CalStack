//! Authentication primitives: Argon2id passwords, token generation/hashing,
//! TOTP and WebAuthn helpers, envelope encryption. Storage lives in
//! `calendar-db`; HTTP session semantics live in `calendar-server`.

pub mod crypto;
pub mod totp;
pub mod webauthn;

pub use crypto::{Crypto, CryptoError};

use argon2::Argon2;
use argon2::password_hash::{
    PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng,
    rand_core::RngCore,
};
use sha2::Digest;
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum AuthError {
    #[error("bad credentials")]
    BadCredentials,
    #[error("hash failure")]
    Hash,
}

/// Argon2id hash of a plaintext password (PHC string format).
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| AuthError::Hash)
}

pub fn verify_password(password: &str, phc_hash: &str) -> Result<(), AuthError> {
    let parsed = PasswordHash::new(phc_hash).map_err(|_| AuthError::Hash)?;
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| AuthError::BadCredentials)
}

/// High-entropy opaque secret, base64url (43 chars from 32 bytes).
pub fn generate_secret() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    base64_url_encode(&bytes)
}

fn base64_url_encode(bytes: &[u8]) -> String {
    // ponytail: hand-rolled base64url to avoid a base64 fork of APIs; base64 crate is available when more is needed.
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        let pad = 3 - chunk.len();
        for i in 0..4 {
            if i < 4 - pad {
                out.push(ALPHA[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            }
        }
    }
    out
}

/// SHA-256 digest for token/session storage (secrets are high-entropy, so a
/// fast unsalted digest is the standard storage choice).
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Session token: 32 random bytes, hex-encoded for the cookie, stored as sha256.
pub fn generate_session_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_round_trip() {
        let hash = hash_password("correct horse").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password("correct horse", &hash).is_ok());
        assert_eq!(
            verify_password("wrong", &hash),
            Err(AuthError::BadCredentials)
        );
        assert!(hash_password("correct horse").unwrap() != hash, "salted");
    }

    #[test]
    fn secret_is_url_safe_and_unique() {
        let a = generate_secret();
        let b = generate_secret();
        assert_eq!(a.len(), 43);
        assert!(!a.contains('+') && !a.contains('/'));
        assert_ne!(a, b);
    }

    #[test]
    fn base64url_matches_known_vectors() {
        // RFC 4648 base64url vectors.
        assert_eq!(base64_url_encode(b""), "");
        assert_eq!(base64_url_encode(b"f"), "Zg");
        assert_eq!(base64_url_encode(b"fo"), "Zm8");
        assert_eq!(base64_url_encode(b"foo"), "Zm9v");
        assert_eq!(base64_url_encode(b"foob"), "Zm9vYg");
        assert_eq!(base64_url_encode(b"fooba"), "Zm9vYmE");
        assert_eq!(base64_url_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            hex_encode(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
