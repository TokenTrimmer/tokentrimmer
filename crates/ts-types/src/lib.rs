//! TypeScript bindings codegen. Tests in this crate emit `.d.ts` files into
//! `bindings/` — CI's bindings-drift-check job fails if generated output differs
//! from committed.

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder_until_ts_rs_wired() {
        // Real codegen lands when wire types stabilize (Week 2-5).
    }
}
