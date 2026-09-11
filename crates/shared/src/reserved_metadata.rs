//! R07: the reserved request-metadata namespace and the caller tag contract.
//!
//! The review's finding: routing conditions include caller-supplied tags
//! (`X-TokenTrimmer-Tag`), and nothing Distinguished authenticated caller
//! metadata from anything else — "end users cannot escape governance by
//! editing a prompt or supplying a forged tenant/customer tag" was not yet
//! true of the tag condition.
//!
//! ## The contract
//!
//! * **Caller tags** (`X-TokenTrimmer-Tag`) remain freeform cost-attribution
//!   labels — but now BOUNDED: at most [`TAG_MAX_BYTES`] bytes, ASCII
//!   printable, no control characters, and MUST NOT start with the
//!   reserved prefix. An invalid tag is DROPPED (the request proceeds
//!   untagged; attribution is best-effort), never rejected outright — the
//!   same fail-open posture the header always had, now with a cardinality
//!   bound that protects routing tables, request_logs storage, and cache
//!   keys from unbounded distinct values.
//! * **The reserved namespace** (`tt.` prefix, case-sensitive lowercase) is
//!   for AUTHENTICATED, gateway-validated metadata only. Today it carries
//!   the workload name (R07): the workload-policy surface delivers
//!   `tt.workload:<name>` as request metadata; the plain caller tag header
//!   can never mint it. Reserved-key delivery channels must be
//!   authenticated (the workload header is validated against the org's
//!   registered policies by the gateway before the routing engine sees it).
//! * Route conditions that gate on the reserved namespace
//!   ([`RouteConditions::workload`]) can therefore only match TRUSTED
//!   input.
//!
//! ## What this is not
//!
//! Not prompt-text parsing (the engine never re-reads prompt text for any
//! tag-like signal), and not yet the full workload-policy registry — the
//! gateway validates and stamps the reserved key; the policy objects
//! (approved model sets, budget owners) live in the control plane. This
//! module is the SHARED contract both sides compile against.

/// The reserved metadata key prefix. Keys in this namespace are minted only
/// by authenticated gateway surfaces; caller input starting with the prefix
/// is rejected as a tag and never reaches routing.
pub const RESERVED_PREFIX: &str = "tt.";

/// The reserved key carrying the trusted workload name (R07).
pub const WORKLOAD_KEY: &str = "tt.workload";

/// Maximum accepted length of a caller tag (bytes).
pub const TAG_MAX_BYTES: usize = 128;

/// Validate a caller-supplied tag. `Ok` tags pass through as attribution
/// labels; `Err` tags are DROPPED by the entry points (best-effort
/// attribution — never a hard request failure).
///
/// Rules: 1..=TAG_MAX_BYTES bytes, ASCII printable (0x21..=0x7E — no spaces,
/// no control characters, no DEL), and NOT the reserved prefix (case
/// matters; `TT.` is not reserved and is simply an ordinary tag).
#[must_use]
pub fn valid_caller_tag(tag: &str) -> bool {
    if tag.is_empty() || tag.len() > TAG_MAX_BYTES {
        return false;
    }
    if tag.starts_with(RESERVED_PREFIX) {
        return false;
    }
    tag.bytes().all(|b| (0x21..=0x7E).contains(&b))
}

/// Validate a workload name as delivered by the authenticated
/// workload-policy surface. Workload names are lowercase slugs
/// (`[a-z0-9][a-z0-9-]{1,62}`) so they cannot collide with the freeform
/// caller-tag grammar beyond the shared prefix, sort stably, and cannot
/// smuggle control characters or the reserved separator into logs or SQL
/// LIKE patterns.
#[must_use]
pub fn valid_workload_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 2 || bytes.len() > 63 {
        return false;
    }
    bytes
        .first()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// The trusted workload name carried on a request, if any. Entry points MUST
/// set this ONLY from an authenticated, policy-validated header — never
/// from `X-TokenTrimmer-Tag` or any prompt-derived value. Returns `None`
/// when the value fails [`valid_workload_name`] (fail closed: an invalid
/// workload name cannot match a workload route, and no route can "accidentally"
/// broaden to unvalidated input).
#[must_use]
pub fn trusted_workload_from(tag: &Option<String>) -> Option<String> {
    // The trusted channel reuses the tag slot with the reserved key: the
    // gateway entry point normalizes `tt.workload:<name>` into
    // `RequestContext::tag` ONLY after validating the name and the caller's
    // policy registration. (A dedicated RequestContext field would break the
    // 25-bind insert contract; the reserved key IS the namespace boundary.)
    let value = tag.as_deref()?;
    let name = value.strip_prefix(RESERVED_PREFIX)?;
    let name = name.strip_prefix("workload").unwrap_or(name);
    let name = name.strip_prefix(':').unwrap_or(name);
    valid_workload_name(name).then(|| name.to_string())
}

/// Extract + validate the caller tag from a raw header value. `None` when
/// absent or invalid (dropped, not rejected — attribution is best-effort).
/// Entry points MUST use this instead of reading `X-TokenTrimmer-Tag` raw:
/// it enforces the bounded-printable-unreserved contract and keeps the tag
/// slot injective with its two (caller | trusted-workload) grammars.
#[must_use]
pub fn caller_tag_from_header(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    valid_caller_tag(value).then(|| value.to_string())
}

/// Combine the caller tag with an authenticated workload name into the
/// single `RequestContext::tag` slot. The workload WINS when both are set:
/// the trusted channel is authoritative for routing, and a caller who sets
/// both is delegating to the policy (the caller tag remains on the raw
/// header for attribution readers that want it). Returns `None` when the
/// workload name fails validation — fail closed for the trusted channel
/// (mismatched policy state must not downgrade into a mystery tag).
#[must_use]
pub fn combine_tags(caller_tag: Option<String>, trusted_workload: Option<&str>) -> Option<String> {
    if let Some(name) = trusted_workload {
        if !valid_workload_name(name) {
            return None;
        }
        return Some(format!("{WORKLOAD_KEY}:{name}"));
    }
    caller_tag
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_tags_are_bounded_printable_and_unreserved() {
        assert!(valid_caller_tag("support"));
        assert!(valid_caller_tag("proj:123"));
        assert!(valid_caller_tag("a".repeat(128).as_str()));
        // Too long / empty.
        assert!(!valid_caller_tag(""));
        assert!(!valid_caller_tag(&"a".repeat(129)));
        // Control characters, spaces, DEL are rejected.
        assert!(!valid_caller_tag("has space"));
        assert!(!valid_caller_tag("tab\tchar"));
        assert!(!valid_caller_tag("nl\nchar"));
        assert!(!valid_caller_tag("del\x7fchar"));
        // The reserved namespace cannot be minted by a caller tag.
        assert!(!valid_caller_tag("tt.workload:support-summary"));
        assert!(!valid_caller_tag("tt.anything"));
        // Case-sensitive: mixed case is ORDINARY, not reserved.
        assert!(valid_caller_tag("TT.not-reserved"));
    }

    #[test]
    fn workload_names_are_lowercase_slug_bounded() {
        assert!(valid_workload_name("support-summary"));
        assert!(valid_workload_name("extraction2"));
        assert!(!valid_workload_name("A"), "single char too short");
        assert!(!valid_workload_name(""));
        assert!(!valid_workload_name(&"x".repeat(64)));
        assert!(!valid_workload_name("Upper"));
        assert!(!valid_workload_name("under_score"));
        assert!(!valid_workload_name("dot.name"));
        assert!(!valid_workload_name("-leading"));
        assert!(valid_workload_name("trailing-"));
        // A trailing hyphen is still a bounded slug — allowed. A LEADING one
        // is rejected by the first-byte rule above.
        assert!(!valid_workload_name("with space"));
    }

    #[test]
    fn trusted_workload_extracts_only_the_reserved_form_with_a_valid_name() {
        // The one delivery form: the reserved key with a colon.
        assert_eq!(
            trusted_workload_from(&Some("tt.workload:support-summary".into())),
            Some("support-summary".into())
        );
        // A plain caller tag NEVER carries a workload — no matter its text.
        assert_eq!(trusted_workload_from(&Some("support-summary".into())), None);
        assert_eq!(trusted_workload_from(&Some("tt.workload".into())), None);
        // The reserved key with an INVALID name fails closed (no match).
        assert_eq!(
            trusted_workload_from(&Some("tt.workload:UPPER".into())),
            None
        );
        assert_eq!(trusted_workload_from(&Some("tt.workload:".into())), None);
        assert_eq!(trusted_workload_from(&Some("tt.workload:x".into())), None);
        assert_eq!(trusted_workload_from(&None), None);
    }
}
