//! `tt-ml-scoring` — in-process ONNX scoring for the learned-compression model.
//!
//! Phase 2b: the model loads in the customer-facing gateway process on the M4 Max
//! (CoreML EP via Metal, no network call) behind an off-by-default feature flag.
//! The `ort` dep + the ~220MB model live ONLY behind `ml-scoring`; the public/Fly
//! gateway builds stay ML-dep-free.
//!
//! # Safe-by-construction (fail-open, never blocks a request)
//! - The session is loaded ONCE on first `score()` call via a `OnceLock`; if the
//!   model path is unset → `Err` (the gateway boots fine, serves deterministic P1b).
//! - **Hard-timeout** — the `score()` call is bounded by `TT_ML_SCORE_TIMEOUT_MS`
//!   (default 50ms); on expiry → `Err` (the caller fails open to deterministic P1b).
//! - **CoreML EP + CPU fallback** — the execution provider chain is `[CoreML, CPU]`;
//!   CoreML uses Metal internally on macOS.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::TensorRef;

/// Environment variable naming the ONNX model path. Required for live scoring;
/// unset → the scorer returns `Err` (the gateway serves deterministic P1b).
pub const MODEL_PATH_ENV: &str = "TT_ML_MODEL_PATH";

/// Hard timeout for a single scoring call (milliseconds). On expiry → fail-open
/// to the deterministic backend. Default 50ms.
pub const DEFAULT_TIMEOUT_MS: u64 = 50;

/// The scoring timeout env (override the default).
pub const TIMEOUT_ENV: &str = "TT_ML_SCORE_TIMEOUT_MS";

/// Error from the scorer — always fail-open (the caller serves deterministic P1b).
#[derive(Debug, thiserror::Error)]
pub enum ScoreError {
    #[error("no model path (set {env})")]
    NoModel { env: &'static str },
    #[error("model load failed: {0}")]
    Load(String),
    #[error("inference error: {0}")]
    Inference(String),
    #[error("timeout ({0}ms) — fail-open to deterministic")]
    Timeout(u64),
}

/// The input text tokenized as token IDs (the caller tokenizes via tt-tokenize
/// before calling `score`; the model expects `input_ids: i64` of shape `[1, seq_len]`).
pub struct ScoreInput<'a> {
    pub token_ids: &'a [i64],
}

/// The output: a per-token keep/drop density in `[0.0, 1.0]`.
pub type KeepDensity = Vec<f32>;

/// The in-process ONNX scorer. The session is cached process-wide via `OnceLock`;
/// `score()` is synchronous (the caller wraps it in `spawn_blocking` if needed).
pub struct Scorer {
    session: OnceLock<Arc<MutexSession>>,
    model_path: Option<PathBuf>,
    timeout_ms: u64,
}

/// A `Session` behind a mutex (`session.run()` takes `&mut self`, so the `Arc<Session>`
/// must be mutex-guarded for the threaded hard-timeout pattern).
struct MutexSession(std::sync::Mutex<Session>);

impl std::fmt::Debug for Scorer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scorer")
            .field("configured", &self.model_path.is_some())
            .field("timeout_ms", &self.timeout_ms)
            .field("loaded", &self.session.get().is_some())
            .finish()
    }
}

impl Scorer {
    #[must_use]
    pub fn new() -> Self {
        let model_path = std::env::var(MODEL_PATH_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let timeout_ms = std::env::var(TIMEOUT_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        Self {
            session: OnceLock::new(),
            model_path,
            timeout_ms,
        }
    }

    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.model_path.is_some()
    }

    fn session(&self) -> Result<&Arc<MutexSession>, ScoreError> {
        if let Some(s) = self.session.get() {
            return Ok(s);
        }
        ort::init().with_name("tt-ml-scoring").commit();
        let path = self.model_path.as_ref().ok_or(ScoreError::NoModel {
            env: MODEL_PATH_ENV,
        })?;
        let mut session = Session::builder()
            .map_err(|e| ScoreError::Load(format!("session builder: {e}")))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| ScoreError::Load(format!("opt level: {e}")))?
            .with_execution_providers([
                ort::ep::CoreML::default().into(),
                ort::ep::CPU::default().into(),
            ])
            .map_err(|e| ScoreError::Load(format!("execution providers: {e}")))?
            .commit_from_file(path)
            .map_err(|e| ScoreError::Load(format!("commit_from_file {path:?}: {e}")))?;
        // Warmup: 3 dummy inferences so the first real call is fast.
        for _ in 0..3 {
            let dummy = ndarray::Array2::<f32>::zeros((1, 1));
            if let Ok(t) = TensorRef::from_array_view(&dummy) {
                let _ = session.run(ort::inputs![t]);
            }
        }
        let arc = Arc::new(MutexSession(std::sync::Mutex::new(session)));
        Ok(self.session.get_or_init(|| arc))
    }

    /// Score a tokenized input. Returns a per-token keep/density. On any error
    /// (no model, load failure, inference error, timeout) → `Err` (fail-open).
    pub fn score(&self, input: &ScoreInput<'_>) -> Result<KeepDensity, ScoreError> {
        let session = self.session()?;

        let ids =
            ndarray::Array2::from_shape_vec((1, input.token_ids.len()), input.token_ids.to_vec())
                .map_err(|e| ScoreError::Inference(format!("input tensor shape: {e}")))?;

        let timeout = std::time::Duration::from_millis(self.timeout_ms);
        let session_arc = Arc::clone(session);
        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let result = (|| {
                let guard = session_arc
                    .0
                    .lock()
                    .map_err(|e| ScoreError::Inference(format!("session mutex: {e}")))?;
                let mut session = guard;
                let tensor = TensorRef::from_array_view(&ids)
                    .map_err(|e| ScoreError::Inference(format!("tensor: {e}")))?;
                let outputs = session
                    .run(ort::inputs![tensor])
                    .map_err(|e| ScoreError::Inference(format!("run: {e}")))?;
                parse_output(&outputs)
            })();
            let _ = tx.send(result);
        });

        match rx.recv_timeout(timeout) {
            Ok(Ok(density)) => Ok(density),
            Ok(Err(e)) => Err(e),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                Err(ScoreError::Timeout(self.timeout_ms))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(ScoreError::Inference("scoring thread panicked".into()))
            }
        }
    }
}

fn parse_output(outputs: &ort::session::SessionOutputs) -> Result<KeepDensity, ScoreError> {
    let (_, value) = outputs
        .iter()
        .next()
        .ok_or_else(|| ScoreError::Inference("model produced no outputs".into()))?;
    let array = value
        .try_extract_array::<f32>()
        .map_err(|e| ScoreError::Inference(format!("extract output: {e}")))?;
    // Sigmoid (logit → [0,1] keep-density).
    Ok(array
        .iter()
        .map(|&logit| 1.0 / (1.0 + (-logit).exp()))
        .collect())
}

impl Default for Scorer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests mutate the process-global env (TIMEOUT_ENV / MODEL_PATH_ENV)
    // + read it via `Scorer::new()`. Under parallel test execution they race
    // (one sets TT_ML_SCORE_TIMEOUT_MS=25 while another removes it + reads the
    // default) → flaky failures. This lock serializes the env-touching tests so
    // each sees a consistent env. (The stdlib `serial_test` crate would do this
    // too; a local Mutex avoids the dep.)
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn scorer_without_model_path_returns_no_model() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(MODEL_PATH_ENV);
        let scorer = Scorer::new();
        assert!(!scorer.is_configured());
        let input = ScoreInput {
            token_ids: &[1, 2, 3],
        };
        let result = scorer.score(&input);
        assert!(
            matches!(result, Err(ScoreError::NoModel { .. })),
            "no model → Err(NoModel); got: {result:?}"
        );
    }

    #[test]
    fn scorer_reads_timeout_from_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var(TIMEOUT_ENV).ok();
        std::env::set_var(TIMEOUT_ENV, "25");
        let scorer = Scorer::new();
        assert_eq!(scorer.timeout_ms, 25);
        if let Some(v) = saved {
            std::env::set_var(TIMEOUT_ENV, v);
        } else {
            std::env::remove_var(TIMEOUT_ENV);
        }
    }

    #[test]
    fn scorer_default_timeout_is_50ms() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var(TIMEOUT_ENV).ok();
        std::env::remove_var(TIMEOUT_ENV);
        let scorer = Scorer::new();
        assert_eq!(scorer.timeout_ms, DEFAULT_TIMEOUT_MS);
        if let Some(v) = saved {
            std::env::set_var(TIMEOUT_ENV, v);
        }
    }
}
