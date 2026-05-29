//! TT_API_KEY validation. v1 just enforces presence + prefix; full
//! verification against the hosted Gateway is delegated to per-tool calls.

use crate::error::McpError;

pub fn validate_api_key(env_var: Option<String>) -> Result<String, McpError> {
    let k = env_var.ok_or_else(|| McpError::Unauthorized("TT_API_KEY missing".into()))?;
    if !k.starts_with("tt_live_") && !k.starts_with("tt_test_") {
        return Err(McpError::Unauthorized("invalid TT_API_KEY prefix".into()));
    }
    Ok(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_key() {
        assert!(matches!(validate_api_key(None).unwrap_err(), McpError::Unauthorized(_)));
    }

    #[test]
    fn rejects_bad_prefix() {
        assert!(matches!(validate_api_key(Some("nope".into())).unwrap_err(), McpError::Unauthorized(_)));
    }

    #[test]
    fn accepts_valid_prefix() {
        assert!(validate_api_key(Some("tt_live_abc".into())).is_ok());
        assert!(validate_api_key(Some("tt_test_xyz".into())).is_ok());
    }
}
