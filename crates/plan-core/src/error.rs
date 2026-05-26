//! Errors surfaced by the Plan replay engine.

use thiserror::Error;

/// Error variants produced by [`crate::replay::replay`] and helpers.
#[derive(Debug, Error)]
pub enum PlanError {
    /// The proposed config contains zero routes — there is nothing to replay.
    #[error("proposed config has no routes")]
    EmptyConfig,

    /// The replay window is invalid (`window_end <= window_start`).
    #[error("invalid plan window: window_end ({end}) must be after window_start ({start})")]
    InvalidWindow {
        /// Window start timestamp the caller supplied (RFC3339).
        start: String,
        /// Window end timestamp the caller supplied (RFC3339).
        end: String,
    },

    /// Bootstrap iterations was set to zero — every CI would be `(0, 0)`,
    /// which is almost certainly a caller mistake.
    #[error("bootstrap_iterations must be > 0")]
    ZeroBootstrapIterations,

    /// An internal invariant was violated (e.g., percentile computation on
    /// an empty slice). Holds a free-form description for the logs.
    #[error("plan replay internal invariant violated: {0}")]
    Internal(String),
}
