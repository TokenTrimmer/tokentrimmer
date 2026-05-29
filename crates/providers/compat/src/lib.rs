//! `tt-provider-compat` — the OpenAI-wire-compatible adapter machinery.
//!
//! Many inference providers (OpenAI itself, plus Mistral, Groq, Together AI,
//! OpenRouter) speak the OpenAI `POST /chat/completions` + `/embeddings` wire
//! format. This crate holds the shared plumbing — request/response translation,
//! SSE streaming, error mapping, the HTTP client config, and the generic
//! [`OpenAICompatibleProvider`] — so each adapter crate is a thin
//! configuration over it.
//!
//! `tt-provider-openai` builds its native adapter on top of this crate (adding
//! the OpenAI pricing catalog); the four BYOK adapters depend only on this
//! crate, not on the full OpenAI adapter.

pub mod client;
pub mod compat;
pub mod errors;
pub mod stream;
pub mod translate;

pub use client::ClientConfig;
pub use compat::{CompatConfig, OpenAICompatibleProvider};
