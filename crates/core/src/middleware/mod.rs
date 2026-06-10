//! HTTP middleware layers for the TokenTrimmer gateway.
//!
//! Each submodule exposes a single, focused piece of middleware:
//!
//! * [`trace`] — attaches a UUID v7 trace-id to every request and injects it
//!   as `X-TokenTrimmer-Trace-Id` in the response headers.
//! * [`auth`] — extracts `Authorization: Bearer tt_live_*`, verifies against
//!   the configured key store, and attaches an `ApiKeyContext` extension for
//!   downstream handlers.
//! * [`argon2_cap`] — per-IP rate cap consulted by [`auth`] right before the
//!   (CPU-expensive) argon2 verify, so a flood of bogus keys is shed with 429
//!   before any hashing work.
//! * [`latency`] — stamps `X-TokenTrimmer-Latency-Ms` on every response.

pub mod argon2_cap;
pub mod auth;
pub mod key_cache;
pub mod latency;
pub mod retrieval;
pub mod trace;
