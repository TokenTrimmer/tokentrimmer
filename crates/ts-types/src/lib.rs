//! PLACEHOLDER — TypeScript bindings codegen is NOT yet implemented.
//!
//! The intent: emit `.d.ts` files for the wire types (via `ts-rs` derives) into
//! a `bindings/` dir, gated by a CI bindings-drift check. None of that exists
//! yet — there are no `ts-rs` derives, no `bindings/` output, and no CI job.
//! This crate is reserved as a workspace slot so the codegen has a home when the
//! wire types are wired up; it ships nothing today. Do not depend on it expecting
//! generated types.

#[cfg(test)]
mod tests {
    /// Placeholder until `ts-rs` codegen is wired (see the module docs). Kept so
    /// the crate isn't empty; replace with real codegen-drift assertions then.
    #[test]
    fn placeholder_until_ts_rs_wired() {}
}
