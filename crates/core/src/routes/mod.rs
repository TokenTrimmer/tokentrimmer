//! HTTP route handlers. One file per endpoint family — keeps each well under
//! the 800-line cap enforced by pre-edit-guard.

pub mod agent_run;
pub mod batches;
pub mod chat;
pub mod embeddings;
pub(crate) mod gateway_tools;
pub mod health;
pub mod messages;
pub mod metrics;
pub mod models;
pub mod preview;
pub mod ready;
pub mod responses;
pub mod routes_api;
pub mod sse;
