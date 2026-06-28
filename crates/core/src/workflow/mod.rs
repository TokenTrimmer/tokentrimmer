//! Workflow engine — definition model, validation, persistence, and execution.
//!
//! In W1a this module exposes only the pure definition types.  Subsequent
//! tasks (W1a Tasks 3–9) will add validate, store, engine, and route
//! submodules.

pub mod executor;
pub mod store;
pub mod types;
pub mod validate;

pub use types::{
    content_hash, BudgetPolicy, Edge, ModelSelection, Node, NodeKind, NodeOutput, OnExceed,
    WorkflowDefinition,
};
