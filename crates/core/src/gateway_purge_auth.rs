//! Domain-separated authorization for cloud-triggered gateway cache erasure.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

const KEY_CONTEXT: &[u8] = b"tokentrimmer:gateway-account-purge:key:v1";
const MESSAGE_CONTEXT: &[u8] = b"tokentrimmer:gateway-account-purge:request:v1\0";
const MAX_VALIDITY_SECS: i64 = 120;
const CLOCK_SKEW_SECS: i64 = 30;

/// HMAC capability verifier derived from the shared deployment root key.
#[derive(Clone)]
pub struct GatewayPurgeAuthorizer {
    key: [u8; 32],
}

impl GatewayPurgeAuthorizer {
    /// Derive an independent v1 subkey from the deployment master key.
    #[must_use]
    pub fn from_master_key(master_key: &[u8; 32]) -> Self {
        let mut derive =
            HmacSha256::new_from_slice(master_key).expect("HMAC accepts every key length");
        derive.update(KEY_CONTEXT);
        Self {
            key: derive.finalize().into_bytes().into(),
        }
    }

    /// Load the root from `TT_MASTER_KEY`. Unset leaves the route disabled;
    /// malformed configuration is an explicit error.
    pub fn from_env() -> Result<Option<Self>, String> {
        let Ok(raw) = std::env::var("TT_MASTER_KEY") else {
            return Ok(None);
        };
        let decoded = hex::decode(raw.trim())
            .map_err(|_| "TT_MASTER_KEY must contain exactly 32 hex-encoded bytes".to_owned())?;
        let master: [u8; 32] = decoded
            .try_into()
            .map_err(|_| "TT_MASTER_KEY must contain exactly 32 hex-encoded bytes".to_owned())?;
        Ok(Some(Self::from_master_key(&master)))
    }

    fn canonical_message(
        task_id: Uuid,
        org_id: Uuid,
        issued_at_unix: i64,
        expires_at_unix: i64,
    ) -> Vec<u8> {
        let fields = format!("{task_id}\n{org_id}\n{issued_at_unix}\n{expires_at_unix}");
        let mut message = Vec::with_capacity(MESSAGE_CONTEXT.len() + fields.len());
        message.extend_from_slice(MESSAGE_CONTEXT);
        message.extend_from_slice(fields.as_bytes());
        message
    }

    /// Produce the lowercase wire signature. Exposed for contract tests and
    /// in-process clients; production gateway code uses [`Self::verify`].
    #[must_use]
    pub fn signature_hex(
        &self,
        task_id: Uuid,
        org_id: Uuid,
        issued_at_unix: i64,
        expires_at_unix: i64,
    ) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("fixed HMAC key is valid");
        mac.update(&Self::canonical_message(
            task_id,
            org_id,
            issued_at_unix,
            expires_at_unix,
        ));
        hex::encode(mac.finalize().into_bytes())
    }

    /// Verify signature and the short, skew-bounded validity window.
    #[must_use]
    pub fn verify(
        &self,
        signature_hex: &str,
        task_id: Uuid,
        org_id: Uuid,
        issued_at_unix: i64,
        expires_at_unix: i64,
        now_unix: i64,
    ) -> bool {
        if org_id.is_nil()
            || expires_at_unix < issued_at_unix
            || expires_at_unix.saturating_sub(issued_at_unix) > MAX_VALIDITY_SECS
            || now_unix < issued_at_unix.saturating_sub(CLOCK_SKEW_SECS)
            || now_unix > expires_at_unix
            || signature_hex.len() != 64
            || !signature_hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return false;
        }
        let Ok(signature) = hex::decode(signature_hex) else {
            return false;
        };
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("fixed HMAC key is valid");
        mac.update(&Self::canonical_message(
            task_id,
            org_id,
            issued_at_unix,
            expires_at_unix,
        ));
        mac.verify_slice(&signature).is_ok()
    }
}

impl std::fmt::Debug for GatewayPurgeAuthorizer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GatewayPurgeAuthorizer")
            .field("key", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_exact_and_validity_is_bounded() {
        let authorizer = GatewayPurgeAuthorizer::from_master_key(&[7; 32]);
        let task = Uuid::from_u128(1);
        let org = Uuid::from_u128(2);
        let signature = authorizer.signature_hex(task, org, 100, 160);
        assert_eq!(
            signature,
            "d51eeaca8199b523aab18e0386c2be18ef0c97a58238ac3c848087af274261bf"
        );
        assert!(authorizer.verify(&signature, task, org, 100, 160, 120));
        assert!(!authorizer.verify(&signature, task, Uuid::from_u128(3), 100, 160, 120));
        assert!(!authorizer.verify(&signature, task, org, 100, 160, 161));
        assert!(!authorizer.verify(&signature.to_uppercase(), task, org, 100, 160, 120));
    }
}
