//! Workflow engine — definition model, validation, persistence, and execution.
//!
//! In W1a this module exposes only the pure definition types.  Subsequent
//! tasks (W1a Tasks 3–9) will add validate, store, engine, and route
//! submodules.

pub mod types;

pub use types::{
    BudgetPolicy, Edge, ModelSelection, Node, NodeKind, NodeOutput, OnExceed,
    WorkflowDefinition, content_hash,
};
