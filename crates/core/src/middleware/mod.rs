//! HTTP middleware layers for the TokenTrimmer gateway.
//!
//! Each submodule exposes a single, focused piece of middleware:
//!
//! * [`trace`] — attaches a UUID v7 trace-id to every request and injects it
//!   as `X-TokenTrimmer-Trace-Id` in the response headers.

pub mod trace;
