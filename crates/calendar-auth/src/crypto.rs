//! Optional envelope encryption for stored secrets (TOTP seeds, notification
//! provider credentials, webhook signing secrets). AES-256-GCM with a random
//! 12-byte nonce prefixed to the ciphertext. Key comes from the environment.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("encryption key not configured (set APP_ENCRYPTION_KEY)")]
    KeyMissing,
    #[error("ciphertext malformed")]
    Malformed,
    #[error("encrypt/decrypt failed")]
    Operation,
}

/// Symmetric key handle. All encryption goes through one key.
pub struct Crypto {
    cipher: Aes256Gcm,
}

impl Crypto {
    /// Parses APP_ENCRYPTION_KEY: 32 bytes as 64 hex chars or base64.
    pub fn from_hex_or_base64(value: Option<&str>) -> Result<Self, CryptoError> {
        let raw = value.ok_or(CryptoError::KeyMissing)?;
        use base64::Engine;
        let bytes = if raw.len() == 64 {
            decode_hex(raw)?
        } else {
            base64::engine::general_purpose::STANDARD
                .decode(raw.trim())
                .map_err(|_| CryptoError::Malformed)?
        };
        if bytes.len() != 32 {
            return Err(CryptoError::Malformed);
        }
        let key = Key::<Aes256Gcm>::from_slice(&bytes);
        Ok(Self {
            cipher: Aes256Gcm::new(key),
        })
    }

    /// Nonce-prefixed AES-256-GCM ciphertext.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = self
            .cipher
            .encrypt(&nonce, plaintext)
            .map_err(|_| CryptoError::Operation)?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        Ok(out)
    }

    pub fn decrypt(&self, blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if blob.len() < 13 {
            return Err(CryptoError::Malformed);
        }
        let (nonce, ct) = blob.split_at(12);
        self.cipher
            .decrypt(Nonce::from_slice(nonce), ct)
            .map_err(|_| CryptoError::Operation)
    }
}

fn decode_hex(s: &str) -> Result<Vec<u8>, CryptoError> {
    if !s.len().is_multiple_of(2) {
        return Err(CryptoError::Malformed);
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| CryptoError::Malformed))
        .collect()
}

/// Hashes a recovery code for totp_secrets.recovery_codes (sha256 hex is enough:
/// codes are 10 chars of ~50 bits of entropy, not guessable offline from the list).
pub fn recovery_code_hash(code: &str) -> String {
    hex_encode(&sha256(format!("recovery:{code}").as_bytes()))
}

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

use sha2::{Digest, Sha256};
fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Crypto {
        Crypto::from_hex_or_base64(Some(
            "11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff",
        ))
        .unwrap()
    }

    #[test]
    fn round_trip() {
        let c = key();
        let blob = c.encrypt(b"hello secret").unwrap();
        assert_ne!(blob, b"hello secret");
        assert_eq!(c.decrypt(&blob).unwrap(), b"hello secret");
        assert!(c.decrypt(&blob[..blob.len() - 1]).is_err());
    }

    #[test]
    fn base64_key() {
        // 32 zero bytes base64
        let c = Crypto::from_hex_or_base64(Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="));
        assert!(c.is_ok());
    }

    #[test]
    fn missing_key_is_error() {
        assert!(matches!(
            Crypto::from_hex_or_base64(None),
            Err(CryptoError::KeyMissing)
        ));
    }
}
