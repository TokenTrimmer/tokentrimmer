//! `tt proxy` — local OpenAI/Anthropic-compatible listener.
//!
//! See `docs/superpowers/specs/2026-05-28-trackB-claude-code-codex-proxy-design.md`.

pub mod config;
pub mod forward;
pub mod listener;
pub mod routes;
pub mod session;
pub mod tui;
