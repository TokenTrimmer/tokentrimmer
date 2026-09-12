//! At-rest response representation, independent of vector selection. Legacy
//! acceptance belongs to the codec's read policy, not the lookup's ranking.
use crate::{
    response_codec::{L2Open, ResponseCodec},
    CacheError,
};

pub(super) fn encode_response_value(
    codec: Option<&ResponseCodec>,
    org_id: uuid::Uuid,
    id: uuid::Uuid,
    response: &[u8],
) -> Result<serde_json::Value, CacheError> {
    match codec {
        Some(codec) => Ok(codec.seal_response_json(org_id, id, response)?),
        None => serde_json::from_slice::<serde_json::Value>(response).map_err(CacheError::Serde),
    }
}

/// Memory-backed L2 also permits opaque fixture bytes. Preserve those only
/// under a legacy policy; neither parse failure nor a disabled codec may
/// bypass required encryption or return a recognized encrypted envelope.
pub(super) fn decode_response_bytes(
    codec: Option<&ResponseCodec>,
    org_id: uuid::Uuid,
    id: uuid::Uuid,
    bytes: &[u8],
) -> Option<Vec<u8>> {
    match (codec, serde_json::from_slice::<serde_json::Value>(bytes)) {
        (Some(codec), Ok(stored)) => decode_response_value(Some(codec), org_id, id, &stored),
        (Some(codec), Err(_)) => codec.allows_legacy_plaintext().then(|| bytes.to_vec()),
        (None, Ok(stored)) if ResponseCodec::is_encrypted_json(&stored) => None,
        (None, _) => Some(bytes.to_vec()),
    }
}

/// None is a miss, never an invitation to treat malformed ciphertext as a
/// provider response. Strict codecs also reject genuine legacy plaintext.
pub(super) fn decode_response_value(
    codec: Option<&ResponseCodec>,
    org_id: uuid::Uuid,
    id: uuid::Uuid,
    stored: &serde_json::Value,
) -> Option<Vec<u8>> {
    match codec {
        Some(codec) => match codec.open_response_json(org_id, id, stored) {
            L2Open::Decrypted(plain) => Some(plain),
            L2Open::Plaintext => serde_json::to_vec(stored).ok(),
            L2Open::Undecryptable => None,
        },
        None => {
            if ResponseCodec::is_encrypted_json(stored) {
                None
            } else {
                serde_json::to_vec(stored).ok()
            }
        }
    }
}
