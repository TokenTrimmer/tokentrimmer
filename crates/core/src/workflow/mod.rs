//! Workflow engine — definition model, validation, persistence, and execution.
//!
//! In W1a this module exposes only the pure definition types.  Subsequent
//! tasks (W1a Tasks 3–9) will add validate, store, engine, and route
//! submodules.

pub mod distill_cache;
pub mod engine;
pub mod estimate;
pub(crate) mod events;
pub mod executor;
pub(crate) mod http;
pub(crate) mod node_run_store;
pub mod quality_gate;
pub(crate) mod schedule;
pub(crate) mod secrets;
pub mod store;
pub mod types;
pub mod validate;

pub use types::{
    content_hash, BudgetPolicy, Edge, ModelSelection, Node, NodeKind, NodeOutput, OnExceed,
    WorkflowDefinition,
};
