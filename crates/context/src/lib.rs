//! Coding-agent context preloader. Builds a repo symbol/import index, ranks
//! files for a task, and assembles a token-budgeted context pack. Fully local
//! and deterministic — no embeddings, no network.
pub mod assemble;
pub mod cache;
pub mod index;
pub mod rank;

#[cfg(test)]
mod smoke {
    #[test]
    fn crate_builds() {
        assert_eq!(2 + 2, 4);
    }
}
