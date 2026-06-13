//! PLACEHOLDER — TypeScript bindings codegen is NOT yet implemented.
//!
//! The intent: emit `.d.ts` files for the wire types (via `ts-rs` derives) into
//! a `bindings/` dir, gated by a CI bindings-drift check. None of that exists
//! yet — there are no `ts-rs` derives and no `bindings/` output. CI only compiles
//! this placeholder crate and emits a warning so the absence of a real drift
//! guard stays visible. Do not depend on it expecting generated types.

#[cfg(test)]
mod tests {
    /// Placeholder until `ts-rs` codegen is wired (see the module docs). Kept so
    /// the crate isn't empty; replace with real codegen-drift assertions then.
    #[test]
    fn placeholder_until_ts_rs_wired() {}
}
