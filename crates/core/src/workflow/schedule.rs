//! Wavefront scheduling helpers for the workflow engine (W3a-1 Tasks 1 & 2).
//!
//! A node is "ready" when it is reachable, not yet done, and every reachable
//! predecessor is already done.  This "reachable-predecessor" readiness rule
//! (rather than raw Kahn in-degree) correctly handles Branch nodes whose
//! skipped arm leaves a phantom incoming edge at a merge node: the merge node's
//! only *reachable* predecessor is the taken arm, so it becomes ready as soon
//! as that arm completes.
//!
//! Task 2 adds [`run_concurrent_model_wave`] which fans out a wave's
//! Model/Agent nodes concurrently via [`futures::future::join_all`].

use std::collections::{HashMap, HashSet};

use futures::future::join_all;

use crate::error::ApiError;
use crate::workflow::executor::{IntelligenceSpec, NodeExecutor};
use crate::workflow::types::NodeOutput;

/// Build a reverse adjacency map (predecessors) from a forward adjacency map.
///
/// `rev_adj[node]` lists every node that has an edge leading TO `node`.
pub(crate) fn build_rev_adj(adj: &HashMap<String, Vec<String>>) -> HashMap<String, Vec<String>> {
    let mut pred: HashMap<String, Vec<String>> = adj.keys().map(|k| (k.clone(), vec![])).collect();
    for (from, tos) in adj {
        for to in tos {
            pred.entry(to.clone()).or_default().push(from.clone());
        }
    }
    pred
}

/// Return the nodes that are ready to run in the current wave.
///
/// A node is ready when:
/// 1. It is in `reachable`.
/// 2. It is NOT yet in `done`.
/// 3. Every predecessor that is itself in `reachable` is already in `done`.
pub(crate) fn ready_nodes(
    reachable: &HashSet<String>,
    done: &HashSet<String>,
    pred: &HashMap<String, Vec<String>>,
) -> Vec<String> {
    reachable
        .iter()
        .filter(|id| {
            if done.contains(*id) {
                return false;
            }
            // Every REACHABLE predecessor must already be done.
            pred.get(*id)
                .map(|preds| {
                    preds
                        .iter()
                        .filter(|p| reachable.contains(*p))
                        .all(|p| done.contains(p))
                })
                .unwrap_or(true)
        })
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Concurrent execution helper (W3a-1 Task 2)
// ---------------------------------------------------------------------------

/// Result from one concurrently-executed Model/Agent node.
pub(crate) struct ConcurrentNodeResult {
    pub node_id: String,
    pub outcome: Result<NodeOutput, ApiError>,
}

/// Run a set of pre-built Model/Agent specs concurrently via [`join_all`].
///
/// Specs must be in stable topo order; results are returned in the same
/// submission order, preserving determinism for the caller's fold step.
///
/// # Concurrency model
///
/// `executor: &dyn NodeExecutor` is `Send + Sync`; references are `Copy`, so
/// each `async move` block captures its own copy of the shared reference
/// without extending its lifetime. All futures complete (via the `join_all`
/// `.await`) before this function returns, so the borrows on `executor` and
/// `specs` remain valid throughout.
///
/// Journal entries and `WfEvent`s are NOT emitted here. The engine folds
/// results single-threadedly in stable topo order after the join — that fold
/// is the sole source of ordering for journal entries and events, which
/// guarantees determinism independent of task-completion order.
pub(crate) async fn run_concurrent_model_wave(
    executor: &dyn NodeExecutor,
    specs: &[(String, IntelligenceSpec)],
) -> Vec<ConcurrentNodeResult> {
    // &dyn NodeExecutor is Copy (it's a fat pointer / reference), so each
    // async-move block captures its own copy — no Arc required.
    let futs: Vec<_> = specs
        .iter()
        .map(|(node_id, spec)| async move {
            ConcurrentNodeResult {
                node_id: node_id.clone(),
                outcome: executor.run_intelligence(node_id.as_str(), spec).await,
            }
        })
        .collect();
    join_all(futs).await
}
