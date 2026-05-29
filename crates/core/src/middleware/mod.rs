//! HTTP middleware layers for the TokenTrimmer gateway.
//!
//! Each submodule exposes a single, focused piece of middleware:
//!
//! * [`trace`] — attaches a UUID v7 trace-id to every request and injects it
//!   as `X-TokenTrimmer-Trace-Id` in the response headers.
//! * [`auth`] — extracts `Authorization: Bearer tt_live_*`, verifies against
//!   the configured key store, and attaches an `ApiKeyContext` extension for
//!   downstream handlers.

pub mod auth;
pub mod retrieval;
pub mod trace;
