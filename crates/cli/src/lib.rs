//! `tt-cli` library — exposes `init`, `proxy`, `retrieval`, and `cost_diff`
//! modules for integration tests and the `tt` binary.

pub mod account;
pub mod advise;
pub mod agent;
pub mod batch;
pub mod budgets;
pub mod catalog;
pub mod chat;
pub mod context;
pub mod cost_diff;
pub mod embed;
pub mod init;
pub mod local_audit;
pub mod plan_apply;
pub mod plan_suggest;
pub mod proxy;
pub mod recipes;
pub mod retrieval;
pub mod route;
pub mod telemetry_window;
pub mod ui;
pub mod workflow;
