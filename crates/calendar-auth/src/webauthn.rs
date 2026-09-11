//! WebAuthn passkey manager: RP configuration plus passkey ceremony wrappers.
//! Ceremony state (and the passkeys under challenge) is held by the caller in
//! memory — challenges are single-use, 10-minute TTL. Persisted passkeys are
//! stored as serialized `Passkey` JSON in `webauthn_credentials.public_key`
//! via calendar-db.

use uuid::Uuid;
use webauthn_rs::prelude::*;

#[derive(Debug, thiserror::Error)]
pub enum WebauthnManagerError {
    #[error("WebAuthn not configured (set WEBAUTHN_RP_ID and WEBAUTHN_ORIGIN)")]
    NotConfigured,
    #[error("WebAuthn ceremony failed: {0}")]
    Ceremony(String),
}

pub struct PasskeyManager {
    webauthn: Webauthn,
}

/// Registration result: the credential id to index, and the serialized
/// Passkey to store verbatim (counter and backup flags travel inside it).
pub struct RegisteredPasskey {
    pub credential_id: Vec<u8>,
    pub passkey_json: Vec<u8>,
}

impl PasskeyManager {
    /// rp_id is the effective domain (e.g. "calendar.example.com"); origin is
    /// the scheme://host[:port] browsers report (e.g. "https://calendar.example.com").
    pub fn new(rp_id: &str, origin: &str) -> Result<Self, WebauthnManagerError> {
        let origin =
            url::Url::parse(origin).map_err(|e| WebauthnManagerError::Ceremony(e.to_string()))?;
        let builder = WebauthnBuilder::new(rp_id, &origin)
            .map_err(|e| WebauthnManagerError::Ceremony(e.to_string()))?;
        let webauthn = builder
            .build()
            .map_err(|e| WebauthnManagerError::Ceremony(e.to_string()))?;
        Ok(Self { webauthn })
    }

    /// Begins passkey registration. Exclude already-registered credential ids
    /// so an authenticator cannot be registered twice.
    pub fn start_registration(
        &self,
        user_unique_id: Uuid,
        user_name: &str,
        display_name: &str,
        exclude: Vec<Vec<u8>>,
    ) -> Result<(CreationChallengeResponse, PasskeyRegistration), WebauthnManagerError> {
        let excluded: Vec<CredentialID> = exclude.into_iter().map(|id| id.into()).collect();
        let excluded = (!excluded.is_empty()).then_some(excluded);
        self.webauthn
            .start_passkey_registration(user_unique_id, user_name, display_name, excluded)
            .map_err(|e| WebauthnManagerError::Ceremony(e.to_string()))
    }

    pub fn finish_registration(
        &self,
        response: &RegisterPublicKeyCredential,
        state: &PasskeyRegistration,
    ) -> Result<RegisteredPasskey, WebauthnManagerError> {
        let passkey = self
            .webauthn
            .finish_passkey_registration(response, state)
            .map_err(|e| WebauthnManagerError::Ceremony(e.to_string()))?;
        let passkey_json = serde_json::to_vec(&passkey)
            .map_err(|e| WebauthnManagerError::Ceremony(e.to_string()))?;
        Ok(RegisteredPasskey {
            credential_id: passkey.cred_id().as_ref().to_vec(),
            passkey_json,
        })
    }

    pub fn start_authentication(
        &self,
        stored: &[Passkey],
    ) -> Result<(RequestChallengeResponse, PasskeyAuthentication), WebauthnManagerError> {
        self.webauthn
            .start_passkey_authentication(stored)
            .map_err(|e| WebauthnManagerError::Ceremony(e.to_string()))
    }

    /// Completes authentication. Returns the matched credential id and the
    /// updated serialization for re-storage (or None when unchanged).
    pub fn finish_authentication(
        &self,
        response: &PublicKeyCredential,
        state: &PasskeyAuthentication,
        stored: &[Passkey],
    ) -> Result<(Vec<u8>, Vec<u8>), WebauthnManagerError> {
        let result = self
            .webauthn
            .finish_passkey_authentication(response, state)
            .map_err(|e| WebauthnManagerError::Ceremony(e.to_string()))?;
        let matched = stored
            .iter()
            .position(|pk| pk.cred_id().as_ref() == result.cred_id().as_ref())
            .map(|idx| stored[idx].clone());
        let updated_json = matched
            .map(|mut pk| {
                let changed = pk.update_credential(&result);
                if changed == Some(false) {
                    return Ok(Vec::new()); // unchanged: nothing to re-store
                }
                serde_json::to_vec(&pk).map_err(|e| WebauthnManagerError::Ceremony(e.to_string()))
            })
            .transpose()?;
        let cred_id = result.cred_id().as_ref().to_vec();
        Ok((cred_id, updated_json.unwrap_or_default()))
    }
}

/// Deserializes stored passkey JSON (calendar-auth never invents these bytes).
pub fn deserialize_passkeys(rows: &[Vec<u8>]) -> Vec<Passkey> {
    rows.iter()
        .filter_map(|blob| serde_json::from_slice(blob).ok())
        .collect()
}
