//! Server-sent event types for streaming workflow runs (W3c Task 1).
//!
//! [`WfEvent`] mirrors the structure and idioms of `RunEvent` in
//! `routes/agent_run.rs`: serde-tagged with dot-style event names, rendered
//! to an [`axum::response::sse::Event`] via [`WfEvent::to_sse`].

use axum::response::sse;

/// One server-sent event from a streaming workflow run.
///
/// Emitted node-by-node as the workflow executes; `NodeDone` carries the
/// live cost burndown so the client can update progress in real time.
#[allow(dead_code)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub(crate) enum WfEvent {
    /// A node has started executing.
    #[serde(rename = "node.start")]
    NodeStart { node_id: String },

    /// A node finished; carries per-node cost + running burndown totals.
    #[serde(rename = "node.done")]
    NodeDone {
        node_id: String,
        /// Cost incurred by this node alone.
        cost_usd: f64,
        /// Cumulative cost of the entire run so far.
        run_cost_usd: f64,
        /// What the run would have cost without TokenTrimmer (same window).
        baseline_cost_usd: f64,
        /// Savings accumulated so far (`baseline - run_cost`).
        saved_usd_so_far: f64,
        /// Remaining budget, if a budget policy is active.
        #[serde(skip_serializing_if = "Option::is_none")]
        budget_remaining_usd: Option<f64>,
    },

    /// The entire workflow run has finished.
    #[serde(rename = "run.done")]
    RunDone {
        /// Terminal status string (e.g. `"completed"`, `"failed"`).
        status: String,
        /// Total cost of the run.
        cost_usd: f64,
        /// Baseline (unoptimised) cost for the run.
        baseline_cost_usd: f64,
        /// Total savings (`baseline - cost`).
        saved_usd: f64,
    },
}

impl WfEvent {
    fn event_name(&self) -> &'static str {
        match self {
            WfEvent::NodeStart { .. } => "node.start",
            WfEvent::NodeDone { .. } => "node.done",
            WfEvent::RunDone { .. } => "run.done",
        }
    }

    /// Render as an axum SSE event (named, JSON data).
    ///
    /// Mirrors `RunEvent::to_sse` from `routes/agent_run.rs` exactly:
    /// `Event::default().event(name).data(serde_json::to_string(self).unwrap_or_default())`.
    #[allow(dead_code)]
    pub(crate) fn to_sse(&self) -> sse::Event {
        sse::Event::default()
            .event(self.event_name())
            .data(serde_json::to_string(self).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sse::Event` is opaque (no public field accessors), so we verify the
    /// event type and payload structure through serde JSON — the same data
    /// that `to_sse` serialises.  We also call `to_sse()` to confirm it
    /// does not panic.
    #[test]
    fn wf_event_node_done_has_correct_name_and_data() {
        let ev = WfEvent::NodeDone {
            node_id: "step-1".to_string(),
            cost_usd: 0.001,
            run_cost_usd: 0.005,
            baseline_cost_usd: 0.010,
            saved_usd_so_far: 0.005,
            budget_remaining_usd: Some(0.995),
        };

        let val = serde_json::to_value(&ev).unwrap();
        assert_eq!(val["type"], "node.done", "serde tag must be node.done");
        assert_eq!(val["node_id"], "step-1");
        assert!(
            val.get("run_cost_usd").is_some(),
            "run_cost_usd must be present"
        );
        assert!(
            val.get("saved_usd_so_far").is_some(),
            "saved_usd_so_far must be present"
        );
        assert!(
            val.get("budget_remaining_usd").is_some(),
            "budget_remaining_usd must be present when Some"
        );

        // Confirm to_sse does not panic.
        let _ = ev.to_sse();
    }

    #[test]
    fn wf_event_run_done_serializes() {
        let ev = WfEvent::RunDone {
            status: "completed".to_string(),
            cost_usd: 0.005,
            baseline_cost_usd: 0.010,
            saved_usd: 0.005,
        };

        let val = serde_json::to_value(&ev).unwrap();
        assert_eq!(val["type"], "run.done", "serde tag must be run.done");
        assert!(val.get("saved_usd").is_some(), "saved_usd must be present");
        assert_eq!(val["status"], "completed");

        let _ = ev.to_sse();
    }

    #[test]
    fn wf_event_node_start_serializes() {
        let ev = WfEvent::NodeStart {
            node_id: "step-0".to_string(),
        };

        let val = serde_json::to_value(&ev).unwrap();
        assert_eq!(val["type"], "node.start");
        assert_eq!(val["node_id"], "step-0");

        let _ = ev.to_sse();
    }

    #[test]
    fn wf_event_node_done_budget_none_omitted() {
        let ev = WfEvent::NodeDone {
            node_id: "n".to_string(),
            cost_usd: 0.0,
            run_cost_usd: 0.0,
            baseline_cost_usd: 0.0,
            saved_usd_so_far: 0.0,
            budget_remaining_usd: None,
        };

        let val = serde_json::to_value(&ev).unwrap();
        assert!(
            val.get("budget_remaining_usd").is_none(),
            "budget_remaining_usd must be omitted when None"
        );
    }
}
