//! Wavefront scheduling helpers for the workflow engine (W3a-1 Task 1).
//!
//! A node is "ready" when it is reachable, not yet done, and every reachable
//! predecessor is already done.  This "reachable-predecessor" readiness rule
//! (rather than raw Kahn in-degree) correctly handles Branch nodes whose
//! skipped arm leaves a phantom incoming edge at a merge node: the merge node's
//! only *reachable* predecessor is the taken arm, so it becomes ready as soon
//! as that arm completes.

use std::collections::{HashMap, HashSet};

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
