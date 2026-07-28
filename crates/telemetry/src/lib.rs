//! OpenTelemetry init + audit-log primitives.
//!
//! The audit log is hash-chained (BLAKE3) and signed (Ed25519) per-org.
//! `tt audit verify` (in `crates/cli`) walks the chain.
//!
//! ## Key public items
//!
//! - [`audit::AuditWriter`] — trait for storage backends.
//! - [`audit::InMemoryAuditWriter`] — in-process writer for tests and CLI.
//! - [`audit::verify_chain`] — standalone chain verifier.
//! - [`audit::AuditEntry`] / [`audit::Actor`] — wire types.

pub mod arr_receipt;
pub mod audit;
pub mod body_capture;
pub mod gen_ai;
pub mod l2_receipt;
pub mod panel_legs;
pub mod propagation;
pub mod request_logs;
pub mod tracing;
pub mod vcr;
pub mod wfr_receipt;
