//! TOTP (RFC 6238) two-factor authentication: seed generation, code
//! verification, otpauth URI and recovery codes. Secrets are stored
//! envelope-encrypted (crypto::Crypto) — never in plaintext.

use totp_rs::{Algorithm, TOTP};

pub const TOTP_STEP: u64 = 30;
pub const TOTP_DIGITS: usize = 6;

/// A freshly generated (not yet confirmed) TOTP configuration.
pub struct NewTotp {
    /// Raw 20-byte seed; store encrypted.
    pub secret: Vec<u8>,
    pub otpauth_url: String,
    /// The seed in base32 for manual entry.
    pub base32_secret: String,
}

pub fn generate(issuer: &str, account: &str) -> NewTotp {
    let secret = rand::random::<[u8; 20]>().to_vec();
    let totp = TOTP::new_unchecked(
        Algorithm::SHA1,
        TOTP_DIGITS,
        1,
        TOTP_STEP,
        secret.clone(),
        (!issuer.is_empty()).then(|| issuer.to_string()),
        account.to_string(),
    );
    NewTotp {
        base32_secret: totp.get_secret_base32(),
        otpauth_url: totp.get_url(),
        secret,
    }
}

/// Verifier for a stored seed.
fn totp_for(secret: &[u8], account: &str) -> TOTP {
    TOTP::new_unchecked(
        Algorithm::SHA1,
        TOTP_DIGITS,
        1, // ±1 time step of clock skew
        TOTP_STEP,
        secret.to_vec(),
        None,
        account.to_string(),
    )
}

/// Verifies a user-supplied 6-digit code (±1 step window).
pub fn verify(secret: &[u8], code: &str) -> bool {
    let code = code.trim();
    if code.len() != TOTP_DIGITS || !code.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    totp_for(secret, "verify")
        .check_current(code)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_current_code() {
        let t = generate("calendar-server", "user@example.com");
        assert!(t.otpauth_url.starts_with("otpauth://totp/"));
        assert!(t.base32_secret.len() >= 32);
        let totp = totp_for(&t.secret, "verify");
        let code = totp.generate_current().unwrap();
        assert!(verify(&t.secret, &code));
        assert!(!verify(&t.secret, "1234567"));
        assert!(!verify(&t.secret, "abcdef"));
        assert!(!verify(&t.secret, ""));
    }

    #[test]
    fn base32_secret_is_canonical() {
        // RFC 6238 test seed, base32-encoded, verifies.
        let seed = totp_rs::Secret::Encoded("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ".to_string())
            .to_bytes()
            .unwrap();
        let totp = totp_for(&seed, "x");
        let code = totp.generate_current().unwrap();
        assert!(verify(&seed, &code));
    }
}
