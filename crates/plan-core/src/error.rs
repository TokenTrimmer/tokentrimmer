//! Errors surfaced by the Plan replay engine.

use thiserror::Error;
use uuid::Uuid;

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

    /// Two enabled routes share a priority and their conditions are not
    /// provably disjoint. Historical Plan inputs do not retain the live
    /// store's creation order, so choosing a winner would be fabricated.
    #[error(
        "enabled routes {first_route_id} and {second_route_id} share priority {priority} and may both match"
    )]
    AmbiguousRoutePriority {
        /// Shared priority whose live tie order is unavailable.
        priority: u32,
        /// First potentially overlapping route.
        first_route_id: Uuid,
        /// Second potentially overlapping route.
        second_route_id: Uuid,
    },

    /// A mirrored Plan condition set could not cross the canonical gateway
    /// route-condition wire boundary.
    #[error("route {route_id} condition contract is invalid: {message}")]
    RouteConditionContract {
        /// Proposed route whose conditions could not be prepared.
        route_id: Uuid,
        /// Serialization or canonical decoder failure.
        message: String,
    },

    /// A mirrored Plan action could not cross the canonical gateway route-action
    /// wire boundary. Replay never projects an action it cannot preserve.
    #[error("route {route_id} action contract is invalid: {message}")]
    RouteActionContract {
        /// Proposed route whose action could not be prepared.
        route_id: Uuid,
        /// Serialization or canonical decoder failure.
        message: String,
    },

    /// An internal invariant was violated (e.g., percentile computation on
    /// an empty slice). Holds a free-form description for the logs.
    #[error("plan replay internal invariant violated: {0}")]
    Internal(String),
}
