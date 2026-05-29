//! Interactive confirmation prompts for `tt init --interactive`.
//! Day-0 stub: the non-interactive path is the priority; interactive prompts
//! are a follow-up.

/// Returns `true` if the user confirms (or if non-interactive).
/// Currently always returns `true` (non-interactive stub).
#[allow(dead_code)]
pub fn confirm(_msg: &str) -> bool {
    true
}
