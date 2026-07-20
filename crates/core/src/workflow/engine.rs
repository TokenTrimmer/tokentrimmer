//! Synchronous DAG workflow orchestrator (W1a Task 6).
//!
//! # Template syntax (Transform nodes + Model/Agent prompts)
//!
//! - `{{input}}` — the Trigger node's output content, coerced to a string.
//!   If the trigger content is a JSON string value the raw string is returned;
//!   other JSON values are serialized compactly.
//! - `{{node_id}}` — the full `content` of the named node's output, coerced
//!   to a string by the same rule.
//! - `{{node_id.field}}` — a single top-level field from a JSON-object output.
//!   Resolves to `""` when the node has not run, the content is not an object,
//!   or the field is absent.
//!
//! Substitution scans left-to-right; unclosed `{{` is passed through as-is.
//!
//! # Branch condition syntax
//!
//! - `{{ref}} == "literal"` — string equality after resolving `{{ref}}`.
//! - `{{ref}} != "literal"` — string inequality.
//! - `{{ref}}` — truthiness: non-empty / non-`"false"` / non-`"null"` /
//!   non-`"0"` string.
//!
//! The literal may be single- or double-quoted.  Numeric comparisons are not
//! supported in W1a; pin a string or use the truthiness form.
//!
//! # Auto model selection
//!
//! `ModelSelection::Auto` is not supported in W1a.  Encountering it fails the
//! run immediately with:
//! `"Auto model selection is not supported in W1a; pin a model or route_ref"`
//!
//! # Budget cap
//!
//! A capped Model/Agent wave is serialized. Before each launch the engine
//! reserves the shared preview estimate when the node has the exact priceable
//! single-turn shape, then settles the reservation against actual node cost
//! before considering its sibling. If the cap is already met or the next
//! reservation does not fit, the run stops with `WfStatus::BudgetExhausted`
//! without invoking that node. A started node's routed provider work can still
//! settle above its directional estimate; provider/invoice hard-ceiling work
//! remains separate.
//!
//! # Branch reachability
//!
//! After a Branch node fires, only the chosen arm's target (`when_true` or
//! `when_false`) is added to the reachable set.  Nodes reachable exclusively
//! through the not-taken arm are silently skipped.  A merge node that has one
//! taken and one skipped incoming edge still executes; template refs to the
//! skipped node resolve to `null`/`""`.
//!
//! # Limitations (W1a MVP)
//!
//! - Uncapped Model/Agent nodes in the same wave run concurrently (W3a-1 Task
//!   2); capped waves serialize reservation/settlement. Control nodes (Trigger,
//!   Transform, Branch, Output) are always sequential.
//! - A single explicit `def.edges` arc from a Branch node is treated as
//!   unconditional; avoid adding explicit outgoing edges from Branch nodes.
//! - The topo order is defensive: if validate somehow missed a cycle the engine
//!   returns `WfStatus::Failed` rather than looping.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, LazyLock,
};

use futures::future::BoxFuture;
use tt_shared::context::SecretString;
use uuid::Uuid;

use crate::routes::agent_run_budget::budget_reached;
use crate::workflow::distill_cache::{CachedDistill, DistillCacheKey};
pub(crate) use crate::workflow::distill_cache::{DistillCacheStore, FlowDocDistillCache, NoCache};
use crate::workflow::environment_variables::required_variable_names;
use crate::workflow::events::WfEvent;
use crate::workflow::executor::{reservation_cost_usd, IntelligenceSpec, NodeExecutor};
use crate::workflow::http::{self as wf_http, HttpReqSpec, DEFAULT_MAX_RESPONSE_BYTES};
use crate::workflow::preflight::{prepare_workflow_tree, PreparedWorkflowTree};
use crate::workflow::schedule;
use crate::workflow::secrets::required_secret_names;
use crate::workflow::types::{ModelSelection, Node, NodeKind, NodeOutput, WorkflowDefinition};

// ---------------------------------------------------------------------------
// Public result types
// ---------------------------------------------------------------------------

/// Terminal status of a workflow run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WfStatus {
    Succeeded,
    Failed,
    BudgetExhausted,
}

/// Returned by [`run_workflow`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WorkflowRunResult {
    pub status: WfStatus,
    pub cost_usd: f64,
    /// Sum of each executed node's `NodeOutput.baseline_cost_usd` — what this
    /// run would have cost without TokenTrimmer optimization.
    pub baseline_cost_usd: f64,
    /// `(baseline_cost_usd - cost_usd).max(0.0)`: USD saved by routing.
    /// Reported for all terminal paths (Succeeded, Failed, BudgetExhausted).
    pub saved_usd: f64,
    /// Named outputs collected from Output nodes (node_id, NodeOutput).
    pub node_outputs: Vec<(String, NodeOutput)>,
    pub error: Option<String>,
}

/// Per-node journal entry passed to the caller-supplied callback after each
/// node completes.  The handler can persist this to `workflow_node_runs`.
#[derive(Debug, Clone)]
pub(crate) struct NodeJournalEntry {
    pub node_id: String,
    /// `"completed"` or `"failed"`.
    pub status: String,
    pub output: Option<serde_json::Value>,
    pub cost_usd: f64,
    pub model_used: Option<String>,
    pub error: Option<String>,
    /// Gateway workflow-node envelope timing. For intelligence nodes this
    /// spans the executor call, not individual provider attempts.
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
}

// ---------------------------------------------------------------------------
// Default max turns for Agent nodes (matches CreateRunRequest)
// ---------------------------------------------------------------------------

const DEFAULT_MAX_TURNS: u32 = 8;
const MAX_SUBWORKFLOW_DEPTH: u32 = 5;
/// Global ceiling on total node executions across the entire run tree (parent
/// workflow + all nested loops and sub-workflows combined).
///
/// Rationale: zero-cost nodes (Transform, Branch, Http) never accrue model
/// spend, so the cost-budget guard cannot bound `max_iters^depth` for nested
/// loops. 10 000 is a generous ceiling — the worst-case zero-cost product
/// 100^5 ≈ 10^10 is halted after ≤ 10 000 executions; a legitimate
/// 50-node, depth-3, 10-iter workflow uses ≤ 5 050 nodes. Adjust upward if
/// product requirements change.
const MAX_TOTAL_NODE_EXECUTIONS: u32 = 10_000;
static EMPTY_WORKFLOW_VARIABLES: LazyLock<BTreeMap<String, String>> = LazyLock::new(BTreeMap::new);

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Run a validated [`WorkflowDefinition`] without environment variables.
/// Environment-bound callers use [`run_workflow_with_variables`] so an exact
/// accepted configuration snapshot is threaded through the whole run tree.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_workflow<'a>(
    executor: &'a dyn NodeExecutor,
    def: &'a WorkflowDefinition,
    inputs: &'a serde_json::Value,
    run_max_cost_usd: Option<f64>,
    journal: impl FnMut(NodeJournalEntry) + Send + 'a,
    events: Option<&'a tokio::sync::mpsc::UnboundedSender<WfEvent>>,
    secrets: &'a HashMap<String, SecretString>,
    depth: u32,
    ancestors: &'a [Uuid],
    cache: &'a dyn DistillCacheStore,
) -> BoxFuture<'a, WorkflowRunResult> {
    run_workflow_with_variables(
        executor,
        def,
        inputs,
        run_max_cost_usd,
        journal,
        events,
        secrets,
        &EMPTY_WORKFLOW_VARIABLES,
        depth,
        ancestors,
        cache,
    )
}

/// Run a validated [`WorkflowDefinition`] to completion with an immutable map
/// of non-secret environment variables accepted by the route.
///
/// `executor` provides the Model/Agent node bridge to the gateway.
/// `inputs` is the workflow's trigger payload.
/// `run_max_cost_usd` is the run-level budget cap (supersedes node-level caps
/// for the stop decision; node caps are still passed to the executor for its
/// own per-node guard).
/// `journal` is called synchronously after every executed node.
/// `events` is an optional channel sink; when `Some`, the engine emits
/// [`WfEvent::NodeStart`] / [`WfEvent::NodeDone`] for every executed node and
/// a terminal [`WfEvent::RunDone`] before returning.  When `None` the sync
/// path is byte-identical.
/// Public entry point — accepts any `impl FnMut + Send + 'a`.
/// Boxes the journal to type-erase it before delegating to
/// [`run_workflow_boxed`], which is the concrete (non-generic) recursive impl.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_workflow_with_variables<'a>(
    executor: &'a dyn NodeExecutor,
    def: &'a WorkflowDefinition,
    inputs: &'a serde_json::Value,
    run_max_cost_usd: Option<f64>,
    journal: impl FnMut(NodeJournalEntry) + Send + 'a,
    events: Option<&'a tokio::sync::mpsc::UnboundedSender<WfEvent>>,
    secrets: &'a HashMap<String, SecretString>,
    variables: &'a BTreeMap<String, String>,
    depth: u32,
    ancestors: &'a [Uuid],
    cache: &'a dyn DistillCacheStore,
) -> BoxFuture<'a, WorkflowRunResult> {
    Box::pin(async move {
        // Load, secret-check, and freeze every nested definition before the
        // root Trigger or any parent work. Runtime recursion reuses this exact
        // tree, avoiding a latest-version race after preflight.
        let prepared = match prepare_workflow_tree(
            executor,
            def,
            secrets,
            variables,
            depth,
            MAX_SUBWORKFLOW_DEPTH,
        )
        .await
        {
            Ok(tree) => Arc::new(tree),
            Err(error) => {
                if let Some(tx) = events {
                    let _ = tx.send(WfEvent::RunDone {
                        status: "failed".to_string(),
                        cost_usd: 0.0,
                        baseline_cost_usd: 0.0,
                        saved_usd: 0.0,
                    });
                }
                return WorkflowRunResult {
                    status: WfStatus::Failed,
                    cost_usd: 0.0,
                    baseline_cost_usd: 0.0,
                    saved_usd: 0.0,
                    node_outputs: vec![],
                    error: Some(error),
                };
            }
        };

        // Seed a fresh execution counter at the root. The Arc is cloned into
        // every recursive call so all nested loops and sub-workflows draw from
        // ONE shared budget.
        let executions = Arc::new(AtomicU32::new(0));
        run_workflow_boxed(
            executor,
            def,
            inputs,
            run_max_cost_usd,
            Box::new(journal),
            events,
            secrets,
            variables,
            prepared,
            depth,
            ancestors,
            executions,
            cache,
        )
        .await
    })
}

/// Private recursive implementation.  Takes an OWNED `Box<dyn FnMut + Send + 'a>`
/// so the function has no *type* parameters — only lifetime parameters, which Rust
/// erases at code-gen.  That gives the monomorphiser a single concrete version of
/// the function, breaking the infinite-instantiation chain that an
/// `impl FnMut + Send` generic parameter would produce.
#[allow(clippy::too_many_arguments)]
fn run_workflow_boxed<'a>(
    executor: &'a dyn NodeExecutor,
    def: &'a WorkflowDefinition,
    inputs: &'a serde_json::Value,
    run_max_cost_usd: Option<f64>,
    journal: Box<dyn FnMut(NodeJournalEntry) + Send + 'a>,
    events: Option<&'a tokio::sync::mpsc::UnboundedSender<WfEvent>>,
    secrets: &'a HashMap<String, SecretString>,
    variables: &'a BTreeMap<String, String>,
    prepared: Arc<PreparedWorkflowTree>,
    depth: u32,
    ancestors: &'a [Uuid],
    executions: Arc<AtomicU32>,
    cache: &'a dyn DistillCacheStore,
) -> BoxFuture<'a, WorkflowRunResult> {
    Box::pin(async move {
        let mut journal = journal;
        // No-op when events is None; Option<&T> is Copy so captured by value.
        let emit = |ev: WfEvent| {
            if let Some(tx) = events {
                let _ = tx.send(ev);
            }
        };

        // ---- 1. Defense-in-depth local Http secret preflight -----------------
        // Whole-tree preparation already checked and froze every definition
        // before the root began. Retain the local check as an invariant at each
        // recursive boundary.
        let required_secrets = match required_secret_names(def) {
            Ok(names) => names,
            Err(errors) => {
                emit(WfEvent::RunDone {
                    status: "failed".to_string(),
                    cost_usd: 0.0,
                    baseline_cost_usd: 0.0,
                    saved_usd: 0.0,
                });
                return WorkflowRunResult {
                    status: WfStatus::Failed,
                    cost_usd: 0.0,
                    baseline_cost_usd: 0.0,
                    saved_usd: 0.0,
                    node_outputs: vec![],
                    error: Some(format!(
                        "workflow secret preflight failed: {}",
                        errors.join("; ")
                    )),
                };
            }
        };
        let missing_secrets = required_secrets
            .into_iter()
            .filter(|name| !secrets.contains_key(name))
            .collect::<Vec<_>>();
        if !missing_secrets.is_empty() {
            emit(WfEvent::RunDone {
                status: "failed".to_string(),
                cost_usd: 0.0,
                baseline_cost_usd: 0.0,
                saved_usd: 0.0,
            });
            return WorkflowRunResult {
                status: WfStatus::Failed,
                cost_usd: 0.0,
                baseline_cost_usd: 0.0,
                saved_usd: 0.0,
                node_outputs: vec![],
                error: Some(format!(
                    "workflow secret preflight failed: missing or unusable secret(s): {}",
                    missing_secrets.join(", ")
                )),
            };
        }

        // ---- 1b. Defense-in-depth local variable preflight -----------------
        let required_variables = match required_variable_names(def) {
            Ok(names) => names,
            Err(errors) => {
                emit(WfEvent::RunDone {
                    status: "failed".to_string(),
                    cost_usd: 0.0,
                    baseline_cost_usd: 0.0,
                    saved_usd: 0.0,
                });
                return WorkflowRunResult {
                    status: WfStatus::Failed,
                    cost_usd: 0.0,
                    baseline_cost_usd: 0.0,
                    saved_usd: 0.0,
                    node_outputs: vec![],
                    error: Some(format!(
                        "workflow variable preflight failed: {}",
                        errors.join("; ")
                    )),
                };
            }
        };
        let missing_variables = required_variables
            .into_iter()
            .filter(|name| !variables.contains_key(name))
            .collect::<Vec<_>>();
        if !missing_variables.is_empty() {
            emit(WfEvent::RunDone {
                status: "failed".to_string(),
                cost_usd: 0.0,
                baseline_cost_usd: 0.0,
                saved_usd: 0.0,
            });
            return WorkflowRunResult {
                status: WfStatus::Failed,
                cost_usd: 0.0,
                baseline_cost_usd: 0.0,
                saved_usd: 0.0,
                node_outputs: vec![],
                error: Some(format!(
                    "workflow variable preflight failed: missing environment variable(s): {}",
                    missing_variables.join(", ")
                )),
            };
        }

        // ---- 2. Find the Trigger node -----------------------------------------
        let trigger_id = match def
            .nodes
            .iter()
            .find(|n| matches!(n.kind, NodeKind::Trigger))
        {
            Some(n) => n.id.clone(),
            None => {
                emit(WfEvent::RunDone {
                    status: "failed".to_string(),
                    cost_usd: 0.0,
                    baseline_cost_usd: 0.0,
                    saved_usd: 0.0,
                });
                return WorkflowRunResult {
                    status: WfStatus::Failed,
                    cost_usd: 0.0,
                    baseline_cost_usd: 0.0,
                    saved_usd: 0.0,
                    node_outputs: vec![],
                    error: Some("workflow has no Trigger node".into()),
                };
            }
        };

        // ---- 3. Build union adjacency list ------------------------------------
        let adj = build_union_adj(def);

        // ---- 4. Topological sort (defensive; validate already checked) --------
        let topo_order = match topo_sort(def, &adj) {
            Ok(order) => order,
            Err(e) => {
                emit(WfEvent::RunDone {
                    status: "failed".to_string(),
                    cost_usd: 0.0,
                    baseline_cost_usd: 0.0,
                    saved_usd: 0.0,
                });
                return WorkflowRunResult {
                    status: WfStatus::Failed,
                    cost_usd: 0.0,
                    baseline_cost_usd: 0.0,
                    saved_usd: 0.0,
                    node_outputs: vec![],
                    error: Some(e),
                };
            }
        };

        // ---- 5. Node lookup map -----------------------------------------------
        let node_map: HashMap<&str, &Node> = def.nodes.iter().map(|n| (n.id.as_str(), n)).collect();

        // ---- 6. Run state -------------------------------------------------------
        let mut outputs: HashMap<String, NodeOutput> = HashMap::new();
        let mut collected_outputs: Vec<(String, NodeOutput)> = Vec::new();
        let mut accrued: f64 = 0.0;
        let mut accrued_baseline: f64 = 0.0;
        // Nodes reachable along taken edges from Trigger.
        let mut reachable: HashSet<String> = HashSet::new();
        reachable.insert(trigger_id.clone());

        // ---- 6. Build wavefront scheduling data --------------------------------
        let topo_index: HashMap<String, usize> = topo_order
            .iter()
            .enumerate()
            .map(|(i, id)| (id.clone(), i))
            .collect();
        let pred = schedule::build_rev_adj(&adj);
        let mut done: HashSet<String> = HashSet::new();

        // ---- 7. Execute via wavefront (concurrent within each wave) ---------------
        loop {
            let mut wave = schedule::ready_nodes(&reachable, &done, &pred);
            if wave.is_empty() {
                break;
            }
            // Stable topo order ensures deterministic fold and event ordering.
            wave.sort_by_key(|id| topo_index.get(id).copied().unwrap_or(usize::MAX));

            // Partition into Model/Agent (concurrent async) and Control (sequential).
            // Control nodes run AFTER the Model/Agent batch so any model outputs in
            // the same wave are available to them (mixed waves are rare but handled
            // deterministically).
            let (model_agent_wave, control_wave): (Vec<String>, Vec<String>) =
                wave.into_iter().partition(|id| {
                    matches!(
                        node_map.get(id.as_str()).map(|n| &n.kind),
                        Some(NodeKind::Model { .. }) | Some(NodeKind::Agent { .. })
                    )
                });

            // ==============================================================
            // A. Concurrent Model/Agent batch
            // ==============================================================
            if !model_agent_wave.is_empty() {
                // Global execution-cap: count this wave's nodes at the serialized
                // pre-launch point. `executions` is Arc<AtomicU32> and is only
                // incremented here (serialized fold) and in recursive boxed calls —
                // NEVER inside the concurrently-spawned model/agent futures.
                let wave_count = model_agent_wave.len() as u32;
                let new_total = executions.fetch_add(wave_count, Ordering::Relaxed) + wave_count;
                if new_total > MAX_TOTAL_NODE_EXECUTIONS {
                    let saved_usd = (accrued_baseline - accrued).max(0.0);
                    emit(WfEvent::RunDone {
                        status: "failed".to_string(),
                        cost_usd: accrued,
                        baseline_cost_usd: accrued_baseline,
                        saved_usd,
                    });
                    return WorkflowRunResult {
                        status: WfStatus::Failed,
                        cost_usd: accrued,
                        baseline_cost_usd: accrued_baseline,
                        saved_usd,
                        node_outputs: collected_outputs,
                        error: Some(format!(
                            "workflow exceeded the maximum total node executions \
                             ({MAX_TOTAL_NODE_EXECUTIONS}) — likely an unbounded \
                             loop or sub-workflow nesting"
                        )),
                    };
                }
                // HARD BUDGET GATE — checked before building or launching the wave.
                // Capped waves receive an additional per-node reservation below.
                if budget_reached(accrued, run_max_cost_usd) {
                    let saved_usd = (accrued_baseline - accrued).max(0.0);
                    emit(WfEvent::RunDone {
                        status: "budget_exhausted".to_string(),
                        cost_usd: accrued,
                        baseline_cost_usd: accrued_baseline,
                        saved_usd,
                    });
                    return WorkflowRunResult {
                        status: WfStatus::BudgetExhausted,
                        cost_usd: accrued,
                        baseline_cost_usd: accrued_baseline,
                        saved_usd,
                        node_outputs: collected_outputs,
                        error: None,
                    };
                }

                // Build IntelligenceSpecs sequentially — reads `outputs` and
                // `trigger_id` which are not touched by the concurrent futures.
                let mut specs: Vec<(String, IntelligenceSpec)> = Vec::new();
                for node_id in &model_agent_wave {
                    let node = match node_map.get(node_id.as_str()) {
                        Some(n) => n,
                        None => {
                            done.insert(node_id.clone());
                            continue;
                        }
                    };
                    match &node.kind {
                        NodeKind::Model {
                            selection,
                            prompt,
                            max_output_tokens,
                            max_cost_usd: node_cap,
                        } => {
                            if matches!(selection, ModelSelection::Auto) {
                                let saved_usd = (accrued_baseline - accrued).max(0.0);
                                emit(WfEvent::RunDone {
                                    status: "failed".to_string(),
                                    cost_usd: accrued,
                                    baseline_cost_usd: accrued_baseline,
                                    saved_usd,
                                });
                                return WorkflowRunResult {
                                    status: WfStatus::Failed,
                                    cost_usd: accrued,
                                    baseline_cost_usd: accrued_baseline,
                                    saved_usd,
                                    node_outputs: collected_outputs,
                                    error: Some(
                                        "Auto model selection is not supported in W1a; \
                                     pin a model or route_ref"
                                            .into(),
                                    ),
                                };
                            }
                            specs.push((
                                node_id.clone(),
                                IntelligenceSpec {
                                    selection: selection.clone(),
                                    prompt: substitute(prompt, &trigger_id, &outputs, variables),
                                    tools: vec![],
                                    max_turns: 1,
                                    max_output_tokens: *max_output_tokens,
                                    max_cost_usd: *node_cap,
                                },
                            ));
                        }
                        NodeKind::Agent {
                            selection,
                            prompt,
                            max_turns,
                            max_output_tokens,
                            max_cost_usd: node_cap,
                            tools,
                        } => {
                            if matches!(selection, ModelSelection::Auto) {
                                let saved_usd = (accrued_baseline - accrued).max(0.0);
                                emit(WfEvent::RunDone {
                                    status: "failed".to_string(),
                                    cost_usd: accrued,
                                    baseline_cost_usd: accrued_baseline,
                                    saved_usd,
                                });
                                return WorkflowRunResult {
                                    status: WfStatus::Failed,
                                    cost_usd: accrued,
                                    baseline_cost_usd: accrued_baseline,
                                    saved_usd,
                                    node_outputs: collected_outputs,
                                    error: Some(
                                        "Auto model selection is not supported in W1a; \
                                     pin a model or route_ref"
                                            .into(),
                                    ),
                                };
                            }
                            specs.push((
                                node_id.clone(),
                                IntelligenceSpec {
                                    selection: selection.clone(),
                                    prompt: substitute(prompt, &trigger_id, &outputs, variables),
                                    tools: tools.clone(),
                                    max_turns: max_turns.unwrap_or(DEFAULT_MAX_TURNS),
                                    max_output_tokens: *max_output_tokens,
                                    max_cost_usd: *node_cap,
                                },
                            ));
                        }
                        _ => unreachable!("partitioned into model_agent_wave above"),
                    }
                }

                // Uncapped waves retain parallel execution. Capped waves launch
                // in stable order so each node's preview reservation can be
                // settled against actual cost before its sibling is considered.
                // This removes sibling-wave overshoot; a started node's routed
                // provider work may still settle above its reservation.
                let mut reservation_blocked = false;
                let results = if let Some(cap) = run_max_cost_usd {
                    let mut settled_for_launch = accrued;
                    let mut sequential = Vec::with_capacity(specs.len());
                    for (node_id, spec) in &specs {
                        if budget_reached(settled_for_launch, Some(cap)) {
                            reservation_blocked = true;
                            break;
                        }
                        let remaining = (cap - settled_for_launch).max(0.0);
                        let dispatch_cap = spec
                            .max_cost_usd
                            .map_or(remaining, |node_cap| node_cap.min(remaining));
                        if let Some(reserved) = reservation_cost_usd(spec) {
                            if !reserved.is_finite() || reserved < 0.0 || reserved > dispatch_cap {
                                reservation_blocked = true;
                                break;
                            }
                        }
                        let mut launch_spec = spec.clone();
                        launch_spec.max_cost_usd = Some(dispatch_cap);
                        emit(WfEvent::NodeStart {
                            node_id: node_id.clone(),
                        });
                        let started_at = chrono::Utc::now();
                        let outcome = executor.run_intelligence(node_id, &launch_spec).await;
                        let finished_at = chrono::Utc::now();
                        let failed = outcome.is_err();
                        if let Ok(out) = &outcome {
                            settled_for_launch += out.cost_usd;
                        }
                        sequential.push(schedule::ConcurrentNodeResult {
                            node_id: node_id.clone(),
                            started_at,
                            finished_at,
                            outcome,
                        });
                        if failed {
                            // Unlike an uncapped concurrent wave, no sibling has
                            // launched yet, so stop rather than creating more spend.
                            break;
                        }
                    }
                    sequential
                } else {
                    // Fix #4: emit NodeStart for all concurrent nodes BEFORE
                    // launch so streaming clients see every start before a done.
                    for (node_id, _) in &specs {
                        emit(WfEvent::NodeStart {
                            node_id: node_id.clone(),
                        });
                    }
                    schedule::run_concurrent_model_wave(executor, &specs).await
                };

                // DETERMINISTIC FOLD: NodeDone events + journal entries emitted
                // in stable topo order, never inside the concurrent futures.
                // Fix #1: drain the whole vec before bailing so successful
                // siblings' costs always accrue even on partial wave failure.
                // WF-9: carry (node_id, message) so a NodeError SSE event can
                // attribute the failure to the offending node (red badge) before
                // the terminal run.done — instead of leaving the node at "…" forever.
                let mut wave_error: Option<(String, String)> = None;
                for schedule::ConcurrentNodeResult {
                    node_id,
                    started_at,
                    finished_at,
                    outcome,
                } in results
                {
                    match outcome {
                        Err(e) => {
                            let message = format!("{e}");
                            journal(NodeJournalEntry {
                                node_id: node_id.clone(),
                                status: "failed".into(),
                                output: None,
                                cost_usd: 0.0,
                                model_used: None,
                                error: Some(message.clone()),
                                started_at,
                                finished_at,
                            });
                            if wave_error.is_none() {
                                wave_error = Some((node_id.clone(), message));
                            }
                        }
                        Ok(out) => {
                            accrued += out.cost_usd;
                            accrued_baseline += out.baseline_cost_usd;
                            let node_cost_usd = out.cost_usd; // f64 is Copy; captured before move
                            journal(NodeJournalEntry {
                                node_id: node_id.clone(),
                                status: "completed".into(),
                                output: Some(out.content.clone()),
                                cost_usd: out.cost_usd,
                                model_used: out.model_used.clone(),
                                error: None,
                                started_at,
                                finished_at,
                            });
                            outputs.insert(node_id.clone(), out);
                            propagate_edges(&node_id, def, &mut reachable);
                            emit(WfEvent::NodeDone {
                                node_id: node_id.clone(),
                                cost_usd: node_cost_usd,
                                run_cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd_so_far: (accrued_baseline - accrued).max(0.0),
                                budget_remaining_usd: run_max_cost_usd.map(|m| m - accrued),
                            });
                        }
                    }
                    done.insert(node_id.clone());
                }
                if let Some((err_node_id, err_msg)) = wave_error {
                    let saved_usd = (accrued_baseline - accrued).max(0.0);
                    // WF-9: attribute the failure to the offending node BEFORE the
                    // terminal run.done so the client can badge it red immediately.
                    emit(WfEvent::NodeError {
                        node_id: err_node_id.clone(),
                        message: err_msg.clone(),
                    });
                    emit(WfEvent::RunDone {
                        status: "failed".to_string(),
                        cost_usd: accrued,
                        baseline_cost_usd: accrued_baseline,
                        saved_usd,
                    });
                    return WorkflowRunResult {
                        status: WfStatus::Failed,
                        cost_usd: accrued,
                        baseline_cost_usd: accrued_baseline,
                        saved_usd,
                        node_outputs: collected_outputs,
                        error: Some(format!("node \"{err_node_id}\" failed: {err_msg}")),
                    };
                }
                if reservation_blocked {
                    let saved_usd = (accrued_baseline - accrued).max(0.0);
                    emit(WfEvent::RunDone {
                        status: "budget_exhausted".to_string(),
                        cost_usd: accrued,
                        baseline_cost_usd: accrued_baseline,
                        saved_usd,
                    });
                    return WorkflowRunResult {
                        status: WfStatus::BudgetExhausted,
                        cost_usd: accrued,
                        baseline_cost_usd: accrued_baseline,
                        saved_usd,
                        node_outputs: collected_outputs,
                        error: None,
                    };
                }
            }

            // ==============================================================
            // B. Sequential Control nodes (Trigger, Transform, Branch, Output)
            // ==============================================================
            for node_id in &control_wave {
                // Global execution-cap: each control node counts as 1 execution.
                // Checked at the serialized top-of-loop point (never inside an
                // async spawn).
                let new_total = executions.fetch_add(1, Ordering::Relaxed) + 1;
                if new_total > MAX_TOTAL_NODE_EXECUTIONS {
                    let saved_usd = (accrued_baseline - accrued).max(0.0);
                    emit(WfEvent::RunDone {
                        status: "failed".to_string(),
                        cost_usd: accrued,
                        baseline_cost_usd: accrued_baseline,
                        saved_usd,
                    });
                    return WorkflowRunResult {
                        status: WfStatus::Failed,
                        cost_usd: accrued,
                        baseline_cost_usd: accrued_baseline,
                        saved_usd,
                        node_outputs: collected_outputs,
                        error: Some(format!(
                            "workflow exceeded the maximum total node executions \
                             ({MAX_TOTAL_NODE_EXECUTIONS}) — likely an unbounded \
                             loop or sub-workflow nesting"
                        )),
                    };
                }
                let node = match node_map.get(node_id.as_str()) {
                    Some(n) => n,
                    None => {
                        done.insert(node_id.clone());
                        continue;
                    }
                };

                emit(WfEvent::NodeStart {
                    node_id: node_id.clone(),
                });
                let node_started_at = chrono::Utc::now();

                match &node.kind {
                    // ---------------------------------------------------------------
                    NodeKind::Trigger => {
                        let out = NodeOutput {
                            content: inputs.clone(),
                            cost_usd: 0.0,
                            baseline_cost_usd: 0.0,
                            model_used: None,
                            doc_vision_saved_est_usd: 0.0,
                        };
                        outputs.insert(node_id.clone(), out);
                        propagate_edges(node_id, def, &mut reachable);
                        // Trigger is not journaled (no model/cost).
                        emit(WfEvent::NodeDone {
                            node_id: node_id.clone(),
                            cost_usd: 0.0,
                            run_cost_usd: accrued,
                            baseline_cost_usd: accrued_baseline,
                            saved_usd_so_far: (accrued_baseline - accrued).max(0.0),
                            budget_remaining_usd: run_max_cost_usd.map(|m| m - accrued),
                        });
                    }

                    // ---------------------------------------------------------------
                    NodeKind::Transform { expr } => {
                        let value = substitute(expr, &trigger_id, &outputs, variables);
                        let out = NodeOutput {
                            content: serde_json::Value::String(value.clone()),
                            cost_usd: 0.0,
                            baseline_cost_usd: 0.0,
                            model_used: None,
                            doc_vision_saved_est_usd: 0.0,
                        };
                        journal(NodeJournalEntry {
                            node_id: node_id.clone(),
                            status: "completed".into(),
                            output: Some(serde_json::Value::String(value)),
                            cost_usd: 0.0,
                            model_used: None,
                            error: None,
                            started_at: node_started_at,
                            finished_at: chrono::Utc::now(),
                        });
                        outputs.insert(node_id.clone(), out);
                        propagate_edges(node_id, def, &mut reachable);
                        emit(WfEvent::NodeDone {
                            node_id: node_id.clone(),
                            cost_usd: 0.0,
                            run_cost_usd: accrued,
                            baseline_cost_usd: accrued_baseline,
                            saved_usd_so_far: (accrued_baseline - accrued).max(0.0),
                            budget_remaining_usd: run_max_cost_usd.map(|m| m - accrued),
                        });
                    }

                    // ---------------------------------------------------------------
                    NodeKind::Branch {
                        cond,
                        when_true,
                        when_false,
                    } => {
                        let taken = if eval_cond(cond, &trigger_id, &outputs, variables) {
                            when_true.clone()
                        } else {
                            when_false.clone()
                        };
                        journal(NodeJournalEntry {
                            node_id: node_id.clone(),
                            status: "completed".into(),
                            output: Some(serde_json::Value::String(taken.clone())),
                            cost_usd: 0.0,
                            model_used: None,
                            error: None,
                            started_at: node_started_at,
                            finished_at: chrono::Utc::now(),
                        });
                        // Chosen arm + any unconditional explicit edges from this node.
                        reachable.insert(taken.clone());
                        propagate_edges(node_id, def, &mut reachable);
                        emit(WfEvent::NodeDone {
                            node_id: node_id.clone(),
                            cost_usd: 0.0,
                            run_cost_usd: accrued,
                            baseline_cost_usd: accrued_baseline,
                            saved_usd_so_far: (accrued_baseline - accrued).max(0.0),
                            budget_remaining_usd: run_max_cost_usd.map(|m| m - accrued),
                        });
                    }

                    // ---------------------------------------------------------------
                    NodeKind::Output => {
                        // Collect the last output from each incoming edge's source.
                        for edge in &def.edges {
                            if edge.to == *node_id {
                                if let Some(src_out) = outputs.get(&edge.from) {
                                    collected_outputs.push((node_id.clone(), src_out.clone()));
                                }
                            }
                        }
                        // Output nodes can theoretically have outgoing edges (to another
                        // Output that aggregates); propagate for completeness.
                        propagate_edges(node_id, def, &mut reachable);
                        emit(WfEvent::NodeDone {
                            node_id: node_id.clone(),
                            cost_usd: 0.0,
                            run_cost_usd: accrued,
                            baseline_cost_usd: accrued_baseline,
                            saved_usd_so_far: (accrued_baseline - accrued).max(0.0),
                            budget_remaining_usd: run_max_cost_usd.map(|m| m - accrued),
                        });
                    }

                    NodeKind::Model { .. } | NodeKind::Agent { .. } => {
                        unreachable!("partitioned into model_agent_wave above")
                    }

                    // Http node: guarded outbound HTTP call (W3b Task 3).
                    NodeKind::Http {
                        method,
                        url,
                        headers,
                        body,
                        max_response_bytes,
                    } => {
                        // SECURITY: Substitute templates using substitute_with_secrets so
                        // {{secrets.NAME}} refs resolve to real values on the wire spec.
                        // The resulting HttpReqSpec may contain secret values and MUST NOT
                        // be written to any journal, NodeOutput.content, or error string.
                        let sub_url = wf_http::substitute_with_secrets(
                            url,
                            &trigger_id,
                            &outputs,
                            secrets,
                            variables,
                        );
                        let sub_headers: Vec<(String, String)> = headers
                            .iter()
                            .map(|(k, v)| {
                                (
                                    k.clone(),
                                    wf_http::substitute_with_secrets(
                                        v,
                                        &trigger_id,
                                        &outputs,
                                        secrets,
                                        variables,
                                    ),
                                )
                            })
                            .collect();
                        let sub_body = body.as_ref().map(|b| {
                            wf_http::substitute_with_secrets(
                                b,
                                &trigger_id,
                                &outputs,
                                secrets,
                                variables,
                            )
                        });

                        let spec = HttpReqSpec {
                            method: method.clone(),
                            url: sub_url,
                            headers: sub_headers,
                            body: sub_body,
                            max_response_bytes: max_response_bytes
                                .unwrap_or(DEFAULT_MAX_RESPONSE_BYTES),
                        };

                        match wf_http::run_http(spec, &def.allowed_hosts).await {
                            Ok(resp) => {
                                // SECURITY CHOKEPOINT: journal + output = RESPONSE ONLY.
                                // NEVER include the substituted url/headers/body (they may
                                // contain secrets). Http is zero-cost (no model call).
                                // Include status so downstream nodes can branch on it.
                                let resp_content = serde_json::json!({
                                    "status": resp.status,
                                    "body": resp.body,
                                });
                                let out = NodeOutput {
                                    content: resp_content.clone(),
                                    cost_usd: 0.0,
                                    baseline_cost_usd: 0.0,
                                    model_used: None,
                                    doc_vision_saved_est_usd: 0.0,
                                };
                                journal(NodeJournalEntry {
                                    node_id: node_id.clone(),
                                    status: "completed".into(),
                                    output: Some(resp_content),
                                    cost_usd: 0.0,
                                    model_used: None,
                                    error: None,
                                    started_at: node_started_at,
                                    finished_at: chrono::Utc::now(),
                                });
                                outputs.insert(node_id.clone(), out);
                                propagate_edges(node_id, def, &mut reachable);
                                emit(WfEvent::NodeDone {
                                    node_id: node_id.clone(),
                                    cost_usd: 0.0,
                                    run_cost_usd: accrued,
                                    baseline_cost_usd: accrued_baseline,
                                    saved_usd_so_far: (accrued_baseline - accrued).max(0.0),
                                    budget_remaining_usd: run_max_cost_usd.map(|m| m - accrued),
                                });
                            }
                            Err(e) => {
                                // SECURITY: HttpError strings are sanitized (no url/headers/secrets).
                                let saved_usd = (accrued_baseline - accrued).max(0.0);
                                let message = format!("http error: {e}");
                                journal(NodeJournalEntry {
                                    node_id: node_id.clone(),
                                    status: "failed".into(),
                                    output: None,
                                    cost_usd: 0.0,
                                    model_used: None,
                                    error: Some(message.clone()),
                                    started_at: node_started_at,
                                    finished_at: chrono::Utc::now(),
                                });
                                // WF-9: attribute the Http-node failure to the node
                                // before the terminal run.done.
                                emit(WfEvent::NodeError {
                                    node_id: node_id.clone(),
                                    message: message.clone(),
                                });
                                emit(WfEvent::RunDone {
                                    status: "failed".to_string(),
                                    cost_usd: accrued,
                                    baseline_cost_usd: accrued_baseline,
                                    saved_usd,
                                });
                                return WorkflowRunResult {
                                    status: WfStatus::Failed,
                                    cost_usd: accrued,
                                    baseline_cost_usd: accrued_baseline,
                                    saved_usd,
                                    node_outputs: collected_outputs,
                                    error: Some(format!("node \"{node_id}\": {message}")),
                                };
                            }
                        }
                    }

                    // Loop: bounded iteration over a body sub-workflow (W3a-2 Task 1).
                    NodeKind::Loop {
                        body_workflow_id,
                        cond,
                        max_iters,
                    } => {
                        // a. Depth guard — BEFORE loading or recursing.
                        if depth >= MAX_SUBWORKFLOW_DEPTH {
                            let saved_usd = (accrued_baseline - accrued).max(0.0);
                            emit(WfEvent::RunDone {
                                status: "failed".to_string(),
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                            });
                            return WorkflowRunResult {
                                status: WfStatus::Failed,
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                                node_outputs: collected_outputs,
                                error: Some(format!(
                                    "loop body nesting exceeds max depth \
                                     {MAX_SUBWORKFLOW_DEPTH}"
                                )),
                            };
                        }
                        // b. Cycle guard — BEFORE loading or recursing.
                        if *body_workflow_id == def.id || ancestors.contains(body_workflow_id) {
                            let saved_usd = (accrued_baseline - accrued).max(0.0);
                            emit(WfEvent::RunDone {
                                status: "failed".to_string(),
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                            });
                            return WorkflowRunResult {
                                status: WfStatus::Failed,
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                                node_outputs: collected_outputs,
                                error: Some("loop body cycle detected".into()),
                            };
                        }
                        // c. Reuse the exact body loaded during root preflight.
                        let child_prepared = Arc::clone(&prepared);
                        let child_def = match prepared.definition(body_workflow_id) {
                            Some(definition) => definition,
                            None => {
                                let saved_usd = (accrued_baseline - accrued).max(0.0);
                                emit(WfEvent::RunDone {
                                    status: "failed".to_string(),
                                    cost_usd: accrued,
                                    baseline_cost_usd: accrued_baseline,
                                    saved_usd,
                                });
                                return WorkflowRunResult {
                                    status: WfStatus::Failed,
                                    cost_usd: accrued,
                                    baseline_cost_usd: accrued_baseline,
                                    saved_usd,
                                    node_outputs: collected_outputs,
                                    error: Some(
                                        "loop body missing from prepared workflow tree".into(),
                                    ),
                                };
                            }
                        };
                        let child_ancestors: Vec<Uuid> = [ancestors, &[def.id]].concat();
                        // d. Loop state.
                        let mut iter_input = inputs.clone();
                        let mut loop_cost = 0.0_f64;
                        let mut loop_baseline = 0.0_f64;
                        let mut last_content = serde_json::Value::Null;
                        let mut failed: Option<(WfStatus, Option<String>)> = None;

                        for _i in 0..*max_iters {
                            // Budget stop — re-check each iteration.
                            if budget_reached(accrued + loop_cost, run_max_cost_usd) {
                                break;
                            }
                            // Cond checked BEFORE body (while-semantics).
                            if !eval_cond(cond, &trigger_id, &outputs, variables) {
                                break;
                            }
                            let remaining =
                                run_max_cost_usd.map(|m| (m - accrued - loop_cost).max(0.0));
                            // Pass None for events: per-iteration child run.done
                            // events must not flood the parent stream.
                            let child = run_workflow_boxed(
                                executor,
                                child_def,
                                &iter_input,
                                remaining,
                                Box::new(|e: NodeJournalEntry| journal(e)),
                                None,
                                secrets,
                                variables,
                                Arc::clone(&child_prepared),
                                depth + 1,
                                &child_ancestors,
                                Arc::clone(&executions),
                                cache,
                            )
                            .await;
                            loop_cost += child.cost_usd;
                            loop_baseline += child.baseline_cost_usd;
                            if child.status != WfStatus::Succeeded {
                                failed = Some((child.status, child.error.clone()));
                                break;
                            }
                            // Thread outputs forward for the next eval_cond + downstream.
                            last_content = serde_json::to_value(&child.node_outputs)
                                .unwrap_or(serde_json::Value::Null);
                            iter_input = last_content.clone();
                            outputs.insert(
                                node_id.clone(),
                                NodeOutput {
                                    content: last_content.clone(),
                                    cost_usd: child.cost_usd,
                                    baseline_cost_usd: child.baseline_cost_usd,
                                    model_used: None,
                                    doc_vision_saved_est_usd: 0.0,
                                },
                            );
                        }

                        // e. Fold loop totals into run accruals.
                        accrued += loop_cost;
                        accrued_baseline += loop_baseline;

                        if let Some((child_status, child_error)) = failed {
                            let saved_usd = (accrued_baseline - accrued).max(0.0);
                            let status_str = if child_status == WfStatus::BudgetExhausted {
                                "budget_exhausted"
                            } else {
                                "failed"
                            };
                            emit(WfEvent::RunDone {
                                status: status_str.to_string(),
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                            });
                            return WorkflowRunResult {
                                status: child_status,
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                                node_outputs: collected_outputs,
                                error: child_error,
                            };
                        }

                        // f. Success: finalize with accumulated totals.
                        let final_out = NodeOutput {
                            content: last_content.clone(),
                            cost_usd: loop_cost,
                            baseline_cost_usd: loop_baseline,
                            model_used: None,
                            doc_vision_saved_est_usd: 0.0,
                        };
                        outputs.insert(node_id.clone(), final_out);
                        journal(NodeJournalEntry {
                            node_id: node_id.clone(),
                            status: "completed".into(),
                            output: Some(last_content),
                            cost_usd: loop_cost,
                            model_used: None,
                            error: None,
                            started_at: node_started_at,
                            finished_at: chrono::Utc::now(),
                        });
                        propagate_edges(node_id, def, &mut reachable);
                        emit(WfEvent::NodeDone {
                            node_id: node_id.clone(),
                            cost_usd: loop_cost,
                            run_cost_usd: accrued,
                            baseline_cost_usd: accrued_baseline,
                            saved_usd_so_far: (accrued_baseline - accrued).max(0.0),
                            budget_remaining_usd: run_max_cost_usd.map(|m| m - accrued),
                        });
                    }

                    // SubWorkflow: recursive child execution (W3a-3 Task 2).
                    NodeKind::SubWorkflow {
                        workflow_id,
                        version: _,
                    } => {
                        // a. Budget gate — re-check before async work.
                        if budget_reached(accrued, run_max_cost_usd) {
                            let saved_usd = (accrued_baseline - accrued).max(0.0);
                            emit(WfEvent::RunDone {
                                status: "budget_exhausted".to_string(),
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                            });
                            return WorkflowRunResult {
                                status: WfStatus::BudgetExhausted,
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                                node_outputs: collected_outputs,
                                error: None,
                            };
                        }
                        // b. Depth guard — BEFORE loading or recursing.
                        if depth >= MAX_SUBWORKFLOW_DEPTH {
                            let saved_usd = (accrued_baseline - accrued).max(0.0);
                            emit(WfEvent::RunDone {
                                status: "failed".to_string(),
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                            });
                            return WorkflowRunResult {
                                status: WfStatus::Failed,
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                                node_outputs: collected_outputs,
                                error: Some(format!(
                                    "sub-workflow nesting exceeds max depth \
                                 {MAX_SUBWORKFLOW_DEPTH}"
                                )),
                            };
                        }
                        // c. Cycle guard — BEFORE recursing.
                        if *workflow_id == def.id || ancestors.contains(workflow_id) {
                            let saved_usd = (accrued_baseline - accrued).max(0.0);
                            emit(WfEvent::RunDone {
                                status: "failed".to_string(),
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                            });
                            return WorkflowRunResult {
                                status: WfStatus::Failed,
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                                node_outputs: collected_outputs,
                                error: Some("sub-workflow cycle detected".into()),
                            };
                        }
                        // d. Reuse the exact child loaded during root preflight.
                        let child_prepared = Arc::clone(&prepared);
                        let child_def = match prepared.definition(workflow_id) {
                            Some(definition) => definition,
                            None => {
                                let saved_usd = (accrued_baseline - accrued).max(0.0);
                                emit(WfEvent::RunDone {
                                    status: "failed".to_string(),
                                    cost_usd: accrued,
                                    baseline_cost_usd: accrued_baseline,
                                    saved_usd,
                                });
                                return WorkflowRunResult {
                                    status: WfStatus::Failed,
                                    cost_usd: accrued,
                                    baseline_cost_usd: accrued_baseline,
                                    saved_usd,
                                    node_outputs: collected_outputs,
                                    error: Some(
                                        "sub-workflow missing from prepared workflow tree".into(),
                                    ),
                                };
                            }
                        };
                        // e-f. Remaining budget; child inputs = parent inputs (MVP).
                        let remaining = run_max_cost_usd.map(|m| (m - accrued).max(0.0));
                        // g. Ancestors for child: parent ancestors + this workflow id.
                        let child_ancestors: Vec<Uuid> = [ancestors, &[def.id]].concat();
                        // Call the non-generic boxed variant directly so the
                        // monomorphiser sees a concrete (non-type-parameterised)
                        // recursive call — no infinite instantiation.
                        let child = run_workflow_boxed(
                            executor,
                            child_def,
                            inputs,
                            remaining,
                            Box::new(|e: NodeJournalEntry| journal(e)),
                            events,
                            secrets,
                            variables,
                            child_prepared,
                            depth + 1,
                            &child_ancestors,
                            Arc::clone(&executions),
                            cache,
                        )
                        .await;
                        // h. Propagate child failure with partial spend.
                        if child.status != WfStatus::Succeeded {
                            accrued += child.cost_usd;
                            accrued_baseline += child.baseline_cost_usd;
                            let saved_usd = (accrued_baseline - accrued).max(0.0);
                            let status_str = if child.status == WfStatus::BudgetExhausted {
                                "budget_exhausted"
                            } else {
                                "failed"
                            };
                            emit(WfEvent::RunDone {
                                status: status_str.to_string(),
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                            });
                            return WorkflowRunResult {
                                status: child.status,
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                                node_outputs: collected_outputs,
                                error: child.error,
                            };
                        }
                        // i. Success: fold child cost+baseline into accrued.
                        //    DO NOT add child.saved_usd — saved derives from
                        //    accrued_baseline-accrued at run end, adding child.saved
                        //    would double-count.
                        let child_cost = child.cost_usd;
                        let child_baseline = child.baseline_cost_usd;
                        let out = NodeOutput {
                            content: serde_json::to_value(&child.node_outputs)
                                .unwrap_or(serde_json::Value::Null),
                            cost_usd: child_cost,
                            baseline_cost_usd: child_baseline,
                            model_used: None,
                            doc_vision_saved_est_usd: 0.0,
                        };
                        accrued += out.cost_usd;
                        accrued_baseline += out.baseline_cost_usd;
                        journal(NodeJournalEntry {
                            node_id: node_id.clone(),
                            status: "completed".into(),
                            output: Some(out.content.clone()),
                            cost_usd: out.cost_usd,
                            model_used: None,
                            error: None,
                            started_at: node_started_at,
                            finished_at: chrono::Utc::now(),
                        });
                        outputs.insert(node_id.clone(), out);
                        propagate_edges(node_id, def, &mut reachable);
                        emit(WfEvent::NodeDone {
                            node_id: node_id.clone(),
                            cost_usd: child_cost,
                            run_cost_usd: accrued,
                            baseline_cost_usd: accrued_baseline,
                            saved_usd_so_far: (accrued_baseline - accrued).max(0.0),
                            budget_remaining_usd: run_max_cost_usd.map(|m| m - accrued),
                        });
                    }

                    // ---------------------------------------------------------------
                    // D6 — Document node: distill the document's text layer to text
                    // the downstream nodes can use. v1 (no cache yet): distills fresh
                    // via the document_lane seam on every run (the per-org content-hash
                    // reuse cache `flow_doc_distill_cache` is a follow-up slice that
                    // skips the sidecar call on a cache hit). Fail-loud: a sidecar
                    // error / disabled sidecar → an error NodeOutput (the node never
                    // silently emits raw bytes — a downstream Model node would be
                    // misled). The isolated `doc_vision_saved_est_usd` is booked $0
                    // in v1 (the saving is realized on the downstream Model node,
                    // which sends text-not-image tokens at the served model's rate;
                    // booking it here would need that model, known only downstream).
                    NodeKind::Document { source, cache_key } => {
                        let _ = cache_key; // TODO D6 cache slice: compose into the cache key.
                                           // v1 distills inline base64 bytes only (the seam's posture);
                                           // a URL source is rejected at validation + never reaches here.
                        let (media_type, data_b64) = match source {
                            tt_shared::messages::DocumentSource::Base64 { media_type, data } => {
                                (media_type.clone(), data.clone())
                            }
                            tt_shared::messages::DocumentSource::Url { .. } => {
                                let err = "document node source must be base64 (URLs are not fetched in v1)";
                                journal(NodeJournalEntry {
                                    node_id: node_id.clone(),
                                    status: "failed".into(),
                                    output: None,
                                    cost_usd: 0.0,
                                    model_used: None,
                                    error: Some(err.to_string()),
                                    started_at: node_started_at,
                                    finished_at: chrono::Utc::now(),
                                });
                                let out = NodeOutput {
                                    content: serde_json::Value::Null,
                                    cost_usd: 0.0,
                                    baseline_cost_usd: 0.0,
                                    model_used: None,
                                    doc_vision_saved_est_usd: 0.0,
                                };
                                outputs.insert(node_id.clone(), out);
                                propagate_edges(node_id, def, &mut reachable);
                                emit(WfEvent::NodeDone {
                                    node_id: node_id.clone(),
                                    cost_usd: 0.0,
                                    run_cost_usd: accrued,
                                    baseline_cost_usd: accrued_baseline,
                                    saved_usd_so_far: (accrued_baseline - accrued).max(0.0),
                                    budget_remaining_usd: run_max_cost_usd.map(|m| m - accrued),
                                });
                                done.insert(node_id.clone());
                                continue;
                            }
                        };
                        // The cache key: blake3(content_bytes) + the optional
                        // caller-supplied `cache_key` (substituted against the
                        // trigger's outputs, like a Transform expr). Per-org
                        // isolation is the store impl's responsibility — the
                        // concrete `DistillCacheStore` is constructed per-run with
                        // the org_id + scopes its get/upsert to that org.
                        let caller_key = cache_key
                            .as_ref()
                            .map(|k| substitute(k, &trigger_id, &outputs, variables));
                        let content_hash = blake3::hash(data_b64.as_bytes()).to_hex().to_string();
                        let key = DistillCacheKey {
                            content_hash,
                            caller_key,
                        };
                        // Cache hit → emit the cached text ($0, no sidecar call).
                        // Fail-open: a cache error → None → distills fresh.
                        if let Some(cached) = cache.get(&key).await {
                            let content = serde_json::Value::String(cached.text.clone());
                            journal(NodeJournalEntry {
                                node_id: node_id.clone(),
                                status: "completed".into(),
                                output: Some(content.clone()),
                                cost_usd: 0.0,
                                model_used: None,
                                error: None,
                                started_at: node_started_at,
                                finished_at: chrono::Utc::now(),
                            });
                            let out = NodeOutput {
                                content,
                                cost_usd: 0.0,
                                baseline_cost_usd: 0.0,
                                model_used: None,
                                doc_vision_saved_est_usd: 0.0,
                            };
                            outputs.insert(node_id.clone(), out);
                            propagate_edges(node_id, def, &mut reachable);
                            emit(WfEvent::NodeDone {
                                node_id: node_id.clone(),
                                cost_usd: 0.0,
                                run_cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd_so_far: (accrued_baseline - accrued).max(0.0),
                                budget_remaining_usd: run_max_cost_usd.map(|m| m - accrued),
                            });
                            done.insert(node_id.clone());
                            continue;
                        }
                        // Cache miss → distill via the gateway's Document Lane
                        // seam (the same extraction a routed chat request runs).
                        // Fail-loud on any sidecar error / disabled sidecar (the
                        // seam returns DistillOutcome::Disabled/ExtractFailed).
                        let harness = crate::document_lane::seam::DistillHarness::from_env();
                        let distilled_outcome = crate::document_lane::seam::distill_part(
                            &harness,
                            &media_type,
                            &data_b64,
                        )
                        .await;
                        let out = match distilled_outcome {
                            crate::document_lane::seam::DistillOutcome::Distilled {
                                text, ..
                            } => {
                                // Upsert the fresh distillation into the cache
                                // (fail-open: an upsert error is swallowed — the
                                // next run re-tries + re-stores). The bookkeeping's
                                // `pages` is not carried by `distill_part`'s outcome
                                // (it returns only the joined text); the cached row
                                // records `pages: 0` + the engine tag (the cache is
                                // a perf optimization — the page count is advisory).
                                cache
                                    .upsert(
                                        &key,
                                        &CachedDistill {
                                            text: text.clone(),
                                            pages: 0,
                                            engine: "document_lane::seam".to_string(),
                                        },
                                    )
                                    .await;
                                let content = serde_json::Value::String(text.clone());
                                journal(NodeJournalEntry {
                                    node_id: node_id.clone(),
                                    status: "completed".into(),
                                    output: Some(content.clone()),
                                    cost_usd: 0.0,
                                    model_used: None,
                                    error: None,
                                    started_at: node_started_at,
                                    finished_at: chrono::Utc::now(),
                                });
                                NodeOutput {
                                    content,
                                    cost_usd: 0.0,
                                    baseline_cost_usd: 0.0,
                                    model_used: None,
                                    doc_vision_saved_est_usd: 0.0,
                                }
                            }
                            crate::document_lane::seam::DistillOutcome::Disabled
                            | crate::document_lane::seam::DistillOutcome::ExtractFailed => {
                                let err = "document node distillation failed (sidecar disabled or errored)";
                                journal(NodeJournalEntry {
                                    node_id: node_id.clone(),
                                    status: "failed".into(),
                                    output: None,
                                    cost_usd: 0.0,
                                    model_used: None,
                                    error: Some(err.to_string()),
                                    started_at: node_started_at,
                                    finished_at: chrono::Utc::now(),
                                });
                                NodeOutput {
                                    content: serde_json::Value::Null,
                                    cost_usd: 0.0,
                                    baseline_cost_usd: 0.0,
                                    model_used: None,
                                    doc_vision_saved_est_usd: 0.0,
                                }
                            }
                        };
                        outputs.insert(node_id.clone(), out);
                        propagate_edges(node_id, def, &mut reachable);
                        emit(WfEvent::NodeDone {
                            node_id: node_id.clone(),
                            cost_usd: 0.0,
                            run_cost_usd: accrued,
                            baseline_cost_usd: accrued_baseline,
                            saved_usd_so_far: (accrued_baseline - accrued).max(0.0),
                            budget_remaining_usd: run_max_cost_usd.map(|m| m - accrued),
                        });
                    }
                }

                done.insert(node_id.clone());
            } // for node_id in &control_wave
        } // loop

        let saved_usd = (accrued_baseline - accrued).max(0.0);
        emit(WfEvent::RunDone {
            status: "completed".to_string(),
            cost_usd: accrued,
            baseline_cost_usd: accrued_baseline,
            saved_usd,
        });
        WorkflowRunResult {
            status: WfStatus::Succeeded,
            cost_usd: accrued,
            baseline_cost_usd: accrued_baseline,
            saved_usd,
            node_outputs: collected_outputs,
            error: None,
        }
    })
}

// ---------------------------------------------------------------------------
// Graph helpers
// ---------------------------------------------------------------------------

/// Build adjacency list (node_id → Vec<node_id>) over the UNION of
/// `def.edges` and each Branch node's `when_true`/`when_false` arcs.
/// This mirrors the cycle-detection graph used by `validate::check_cycles`.
fn build_union_adj(def: &WorkflowDefinition) -> HashMap<String, Vec<String>> {
    let mut adj: HashMap<String, Vec<String>> = def
        .nodes
        .iter()
        .map(|n| (n.id.clone(), Vec::new()))
        .collect();

    for edge in &def.edges {
        if adj.contains_key(&edge.from) {
            adj.get_mut(&edge.from).unwrap().push(edge.to.clone());
        }
    }

    for node in &def.nodes {
        if let NodeKind::Branch {
            when_true,
            when_false,
            ..
        } = &node.kind
        {
            if adj.contains_key(&node.id) {
                if adj.contains_key(when_true) {
                    adj.get_mut(&node.id).unwrap().push(when_true.clone());
                }
                if adj.contains_key(when_false) {
                    adj.get_mut(&node.id).unwrap().push(when_false.clone());
                }
            }
        }
    }

    adj
}

/// Kahn's topological sort over `adj`.  Returns the sorted node ids or an
/// error string if a cycle is detected.
fn topo_sort(
    def: &WorkflowDefinition,
    adj: &HashMap<String, Vec<String>>,
) -> Result<Vec<String>, String> {
    let mut in_degree: HashMap<&str, usize> =
        def.nodes.iter().map(|n| (n.id.as_str(), 0usize)).collect();

    for neighbors in adj.values() {
        for nbr in neighbors {
            if let Some(d) = in_degree.get_mut(nbr.as_str()) {
                *d += 1;
            }
        }
    }

    let mut queue: VecDeque<String> = def
        .nodes
        .iter()
        .filter(|n| *in_degree.get(n.id.as_str()).unwrap_or(&1) == 0)
        .map(|n| n.id.clone())
        .collect();

    let mut order: Vec<String> = Vec::with_capacity(def.nodes.len());

    while let Some(id) = queue.pop_front() {
        order.push(id.clone());
        if let Some(neighbors) = adj.get(&id) {
            for nbr in neighbors.clone() {
                if let Some(d) = in_degree.get_mut(nbr.as_str()) {
                    *d -= 1;
                    if *d == 0 {
                        queue.push_back(nbr.clone());
                    }
                }
            }
        }
    }

    if order.len() < def.nodes.len() {
        Err(format!(
            "workflow contains a cycle ({} of {} nodes could not be sorted)",
            def.nodes.len() - order.len(),
            def.nodes.len()
        ))
    } else {
        Ok(order)
    }
}

/// Propagate reachability along all explicit `def.edges` from `node_id`.
/// Called for every node kind (even Branch — the Branch arm propagation is
/// done separately in the main loop before calling this).
fn propagate_edges(node_id: &str, def: &WorkflowDefinition, reachable: &mut HashSet<String>) {
    for edge in &def.edges {
        if edge.from == node_id {
            reachable.insert(edge.to.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// Template substitution
// ---------------------------------------------------------------------------

/// Substitute `{{...}}` references in `template` using the accumulated node
/// outputs.  `trigger_id` is the canonical name for the `{{input}}` alias.
fn substitute(
    template: &str,
    trigger_id: &str,
    outputs: &HashMap<String, NodeOutput>,
    variables: &BTreeMap<String, String>,
) -> String {
    let mut result = String::with_capacity(template.len() + 16);
    let mut remaining = template;

    while let Some(open) = remaining.find("{{") {
        result.push_str(&remaining[..open]);
        remaining = &remaining[open + 2..];

        if let Some(close) = remaining.find("}}") {
            let ref_str = remaining[..close].trim();
            let resolved = resolve_ref(ref_str, trigger_id, outputs, variables);
            result.push_str(&resolved);
            remaining = &remaining[close + 2..];
        } else {
            // Unclosed `{{` — emit as-is and stop scanning.
            result.push_str("{{");
            break;
        }
    }
    result.push_str(remaining);
    result
}

/// Resolve a single `{{ref}}` token.
///
/// - `"input"` → the trigger node's content.
/// - `"node_id"` → the full content of that node.
/// - `"node_id.field"` → a top-level JSON object field of that node's content.
/// - `"secrets.*"` → **always** `"***"` (redaction marker, never the real value).
///   Secrets are resolved exclusively in `wf_http::substitute_with_secrets` so
///   that Model/Agent prompts, Transform exprs, and Branch conditions are always
///   secret-free.
fn resolve_ref(
    ref_str: &str,
    trigger_id: &str,
    outputs: &HashMap<String, NodeOutput>,
    variables: &BTreeMap<String, String>,
) -> String {
    // Split on the first `.` to allow `node.field`.
    let (node_part, field_part) = match ref_str.find('.') {
        Some(pos) => (&ref_str[..pos], Some(&ref_str[pos + 1..])),
        None => (ref_str, None),
    };

    // SECURITY: `{{secrets.*}}` / `{{secrets}}` must never return a real secret
    // value from the shared substitution path. Return an explicit redaction
    // marker so callers see "***" rather than "" (which could be confused with
    // "the secret is an empty string").
    if node_part == "secrets" {
        return "***".to_string();
    }
    if node_part == "variables" {
        return field_part
            .and_then(|name| variables.get(name))
            .cloned()
            .unwrap_or_default();
    }

    // `{{input}}` is an alias for the Trigger node.
    let node_key = if node_part == "input" {
        trigger_id
    } else {
        node_part
    };

    let content = match outputs.get(node_key) {
        Some(out) => &out.content,
        None => return String::new(),
    };

    match field_part {
        None => json_to_string(content),
        Some(field) => match content {
            serde_json::Value::Object(map) => {
                map.get(field).map(json_to_string).unwrap_or_default()
            }
            _ => String::new(),
        },
    }
}

/// Coerce a JSON value to a plain string for template output.
/// String values are unwrapped; other values are compactly serialized.
fn json_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Branch condition evaluation
// ---------------------------------------------------------------------------

/// Evaluate a branch condition string against accumulated outputs.
///
/// Supported forms:
/// - `{{ref}} == "literal"` — string equality.
/// - `{{ref}} != "literal"` — string inequality.
/// - `{{ref}}` — truthiness: non-empty / non-`"false"` / non-`"null"` /
///   non-`"0"` string.
fn eval_cond(
    cond: &str,
    trigger_id: &str,
    outputs: &HashMap<String, NodeOutput>,
    variables: &BTreeMap<String, String>,
) -> bool {
    let cond = cond.trim();

    if let Some((lhs, rhs)) = cond.split_once(" == ") {
        let lhs_val = substitute(lhs.trim(), trigger_id, outputs, variables);
        let rhs_val = strip_quotes(rhs.trim());
        return lhs_val == rhs_val;
    }

    if let Some((lhs, rhs)) = cond.split_once(" != ") {
        let lhs_val = substitute(lhs.trim(), trigger_id, outputs, variables);
        let rhs_val = strip_quotes(rhs.trim());
        return lhs_val != rhs_val;
    }

    // Truthiness fallback.
    let resolved = substitute(cond, trigger_id, outputs, variables);
    is_truthy(&resolved)
}

/// Strip matching single or double quotes from a literal value.
fn strip_quotes(s: &str) -> String {
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Falsy strings: empty, `"false"`, `"null"`, `"0"`.  Everything else is truthy.
fn is_truthy(s: &str) -> bool {
    !matches!(s, "" | "false" | "null" | "0")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::json;
    use tt_shared::context::SecretString;
    use uuid::Uuid;

    use super::*;
    use crate::{
        error::ApiError,
        workflow::types::{BudgetPolicy, Edge, ModelSelection, Node, NodeKind, WorkflowDefinition},
    };

    // ---- Stub NodeExecutor --------------------------------------------------

    /// A test-only executor with scripted per-node responses.
    struct StubExecutor {
        /// node_id → NodeOutput to return on the next call for that node.
        responses: HashMap<String, NodeOutput>,
        /// Append-only call log: (node_id, prompt).
        calls: std::sync::Mutex<Vec<(String, String)>>,
        /// Per-node completion caps captured from the engine's intelligence
        /// specs before an executor/provider path sees them.
        output_caps: std::sync::Mutex<Vec<(String, Option<u32>)>>,
        /// Effective per-node cost caps after the engine applies remaining run
        /// budget. This is the value the gateway dispatch guard receives.
        cost_caps: std::sync::Mutex<Vec<(String, Option<f64>)>>,
        /// workflow_id → WorkflowDefinition registry for sub-workflow loading tests.
        subworkflows: HashMap<Uuid, WorkflowDefinition>,
    }

    impl StubExecutor {
        fn new(responses: Vec<(&str, NodeOutput)>) -> Self {
            StubExecutor {
                responses: responses
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
                calls: std::sync::Mutex::new(Vec::new()),
                output_caps: std::sync::Mutex::new(Vec::new()),
                cost_caps: std::sync::Mutex::new(Vec::new()),
                subworkflows: HashMap::new(),
            }
        }

        fn called_nodes(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(id, _)| id.clone())
                .collect()
        }

        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }

        fn output_caps(&self) -> Vec<(String, Option<u32>)> {
            self.output_caps.lock().unwrap().clone()
        }

        fn cost_caps(&self) -> Vec<(String, Option<f64>)> {
            self.cost_caps.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl NodeExecutor for StubExecutor {
        async fn run_intelligence(
            &self,
            node_id: &str,
            spec: &IntelligenceSpec,
        ) -> Result<NodeOutput, ApiError> {
            self.calls
                .lock()
                .unwrap()
                .push((node_id.to_string(), spec.prompt.clone()));
            self.output_caps
                .lock()
                .unwrap()
                .push((node_id.to_string(), spec.max_output_tokens));
            self.cost_caps
                .lock()
                .unwrap()
                .push((node_id.to_string(), spec.max_cost_usd));
            self.responses
                .get(node_id)
                .cloned()
                .ok_or_else(|| ApiError::Internal(format!("stub: no response for {node_id}")))
        }

        async fn load_subworkflow(&self, id: Uuid) -> Result<WorkflowDefinition, ApiError> {
            self.subworkflows
                .get(&id)
                .cloned()
                .ok_or_else(|| ApiError::NotFound(format!("stub: no subworkflow with id {id}")))
        }
    }

    // ---- Workflow definition helpers ----------------------------------------

    /// T → m1 → m2 → o (linear two-model chain)
    fn make_sequential_def() -> WorkflowDefinition {
        WorkflowDefinition {
            triggers: vec![],
            id: Uuid::nil(),
            version: 1,
            name: "seq".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "m1".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub-model".into(),
                        },
                        prompt: "{{input}}".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "m2".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub-model".into(),
                        },
                        prompt: "{{m1}}".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "m1".into(),
                    map: None,
                },
                Edge {
                    from: "m1".into(),
                    to: "m2".into(),
                    map: None,
                },
                Edge {
                    from: "m2".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        }
    }

    /// T → br (cond: {{input}} == "yes") → m_yes / m_no → o
    fn make_branch_def() -> WorkflowDefinition {
        WorkflowDefinition {
            triggers: vec![],
            id: Uuid::nil(),
            version: 1,
            name: "branch".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "br".into(),
                    kind: NodeKind::Branch {
                        cond: r#"{{input}} == "yes""#.into(),
                        when_true: "m_yes".into(),
                        when_false: "m_no".into(),
                    },
                },
                Node {
                    id: "m_yes".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub-model".into(),
                        },
                        prompt: "yes path".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "m_no".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub-model".into(),
                        },
                        prompt: "no path".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "br".into(),
                    map: None,
                },
                Edge {
                    from: "m_yes".into(),
                    to: "o".into(),
                    map: None,
                },
                Edge {
                    from: "m_no".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        }
    }

    /// T → tr (transform: "{{input}} processed") → m1 (prompt: "{{tr}}") → o
    fn make_transform_def() -> WorkflowDefinition {
        WorkflowDefinition {
            triggers: vec![],
            id: Uuid::nil(),
            version: 1,
            name: "transform".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "tr".into(),
                    kind: NodeKind::Transform {
                        expr: "{{input}} processed".into(),
                    },
                },
                Node {
                    id: "m1".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub-model".into(),
                        },
                        prompt: "{{tr}}".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "tr".into(),
                    map: None,
                },
                Edge {
                    from: "tr".into(),
                    to: "m1".into(),
                    map: None,
                },
                Edge {
                    from: "m1".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        }
    }

    // ---- TDD: write tests FIRST, verify they compile before implementing ----

    /// sequential: trigger → m1 → m2 → output
    /// Both models run, costs accrue, status Succeeded.
    #[tokio::test]
    async fn test_sequential_run() {
        let def = make_sequential_def();
        let stub = StubExecutor::new(vec![
            (
                "m1",
                NodeOutput {
                    content: json!("response_1"),
                    cost_usd: 0.10,
                    baseline_cost_usd: 0.0,
                    model_used: Some("haiku".into()),
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("response_2"),
                    cost_usd: 0.15,
                    baseline_cost_usd: 0.0,
                    model_used: Some("haiku".into()),
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
        ]);

        let mut journal_entries: Vec<NodeJournalEntry> = Vec::new();
        let result = run_workflow(
            &stub,
            &def,
            &json!("hello"),
            None,
            |e| journal_entries.push(e),
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        assert!(
            (result.cost_usd - 0.25).abs() < 1e-9,
            "expected 0.25 total cost, got {}",
            result.cost_usd
        );
        assert_eq!(
            stub.called_nodes(),
            vec!["m1", "m2"],
            "both models must run in order"
        );
        // m1 and m2 each emit one journal entry (Trigger and Output do not).
        let node_ids: Vec<_> = journal_entries.iter().map(|e| e.node_id.as_str()).collect();
        assert_eq!(node_ids, vec!["m1", "m2"]);
        assert!(journal_entries
            .iter()
            .all(|entry| entry.finished_at >= entry.started_at));
    }

    #[tokio::test]
    async fn model_and_agent_output_caps_reach_the_executor_intelligence_specs() {
        let mut def = make_sequential_def();
        def.nodes[1].kind = NodeKind::Agent {
            selection: ModelSelection::Model {
                model: "stub-model".into(),
            },
            prompt: "{{input}}".into(),
            max_turns: Some(1),
            max_output_tokens: Some(23),
            max_cost_usd: None,
            tools: vec![],
        };
        if let NodeKind::Model {
            max_output_tokens, ..
        } = &mut def.nodes[2].kind
        {
            *max_output_tokens = Some(41);
        }
        let stub = StubExecutor::new(vec![
            (
                "m1",
                NodeOutput {
                    content: json!("response_1"),
                    cost_usd: 0.0,
                    baseline_cost_usd: 0.0,
                    model_used: Some("stub-model".into()),
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("response_2"),
                    cost_usd: 0.0,
                    baseline_cost_usd: 0.0,
                    model_used: Some("stub-model".into()),
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
        ]);

        let result = run_workflow(
            &stub,
            &def,
            &json!("hello"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        assert_eq!(
            stub.output_caps(),
            vec![("m1".into(), Some(23)), ("m2".into(), Some(41))]
        );
    }

    /// budget_cap: after m1 (cost 0.25) the cap of 0.20 is exceeded;
    /// m2 is refused → BudgetExhausted; journal has exactly 1 entry (m1).
    #[tokio::test]
    async fn test_budget_cap() {
        let def = make_sequential_def();
        let stub = StubExecutor::new(vec![
            (
                "m1",
                NodeOutput {
                    content: json!("r1"),
                    cost_usd: 0.25,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("r2"),
                    cost_usd: 0.25,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
        ]);

        let mut journal_entries: Vec<NodeJournalEntry> = Vec::new();
        // cap = 0.20: before m1 accrued=0.0 < 0.20 → runs; after m1 accrued=0.25 >= 0.20
        // → before m2 budget_reached=true → BudgetExhausted.
        let result = run_workflow(
            &stub,
            &def,
            &json!("hi"),
            Some(0.20),
            |e| journal_entries.push(e),
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::BudgetExhausted);
        assert!(
            (result.cost_usd - 0.25).abs() < 1e-9,
            "cost must reflect the one node that ran"
        );
        assert_eq!(
            stub.called_nodes(),
            vec!["m1"],
            "m2 must not be called after budget is exhausted"
        );
        assert_eq!(journal_entries.len(), 1, "only m1 should be journaled");
        assert_eq!(journal_entries[0].node_id, "m1");
    }

    /// branch: a Branch node routes on the trigger input; only the chosen
    /// arm's model must run; the other must NOT be called.
    #[tokio::test]
    async fn test_branch_takes_correct_arm() {
        let def = make_branch_def();
        let stub = StubExecutor::new(vec![
            (
                "m_yes",
                NodeOutput {
                    content: json!("yes_out"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
            (
                "m_no",
                NodeOutput {
                    content: json!("no_out"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
        ]);

        // Input "yes" → cond `{{input}} == "yes"` is true → when_true = m_yes.
        let mut journal_entries: Vec<NodeJournalEntry> = Vec::new();
        let result = run_workflow(
            &stub,
            &def,
            &json!("yes"),
            None,
            |e| journal_entries.push(e),
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        let called = stub.called_nodes();
        assert!(
            called.contains(&"m_yes".to_string()),
            "m_yes must run on 'yes' input; called: {called:?}"
        );
        assert!(
            !called.contains(&"m_no".to_string()),
            "m_no must NOT run on 'yes' input; called: {called:?}"
        );
    }

    /// transform: a Transform node maps an upstream output into a downstream
    /// model prompt; assert the substituted value propagated correctly.
    #[tokio::test]
    async fn test_transform_propagates_value() {
        let def = make_transform_def();
        let stub = StubExecutor::new(vec![(
            "m1",
            NodeOutput {
                content: json!("model_out"),
                cost_usd: 0.10,
                baseline_cost_usd: 0.0,
                model_used: None,
                doc_vision_saved_est_usd: 0.0,
            },
        )]);

        let mut journal_entries: Vec<NodeJournalEntry> = Vec::new();
        let result = run_workflow(
            &stub,
            &def,
            &json!("hello"),
            None,
            |e| journal_entries.push(e),
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        let calls = stub.calls();
        assert_eq!(calls.len(), 1, "exactly one model call");
        // The Transform node produced "hello processed"; the model prompt
        // `{{tr}}` should have been substituted to that value.
        assert_eq!(
            calls[0].1, "hello processed",
            "transform output must propagate into the model prompt"
        );
    }

    // ---- Unit tests for substitution and condition helpers ------------------

    #[test]
    fn substitute_input_alias() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "t".into(),
            NodeOutput {
                content: json!("world"),
                ..Default::default()
            },
        );
        assert_eq!(
            substitute("hello {{input}}", "t", &outputs, &BTreeMap::new()),
            "hello world"
        );
    }

    #[test]
    fn substitute_node_field() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "n1".into(),
            NodeOutput {
                content: json!({"answer": "42"}),
                ..Default::default()
            },
        );
        assert_eq!(
            substitute("{{n1.answer}}", "t", &outputs, &BTreeMap::new()),
            "42"
        );
    }

    #[test]
    fn substitute_missing_ref_is_empty() {
        let outputs = HashMap::new();
        assert_eq!(
            substitute("{{missing}}", "t", &outputs, &BTreeMap::new()),
            ""
        );
    }

    #[test]
    fn substitute_resolves_exact_environment_variable() {
        let variables = BTreeMap::from([("REGION".into(), "us-east".into())]);
        assert_eq!(
            substitute(
                "deploy {{variables.REGION}}",
                "t",
                &HashMap::new(),
                &variables,
            ),
            "deploy us-east"
        );
        assert_eq!(
            substitute("{{variables.MISSING}}", "t", &HashMap::new(), &variables,),
            ""
        );
    }

    #[test]
    fn eval_cond_equality_true() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "t".into(),
            NodeOutput {
                content: json!("yes"),
                ..Default::default()
            },
        );
        assert!(eval_cond(
            r#"{{input}} == "yes""#,
            "t",
            &outputs,
            &BTreeMap::new()
        ));
    }

    #[test]
    fn eval_cond_equality_false() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "t".into(),
            NodeOutput {
                content: json!("no"),
                ..Default::default()
            },
        );
        assert!(!eval_cond(
            r#"{{input}} == "yes""#,
            "t",
            &outputs,
            &BTreeMap::new()
        ));
    }

    #[test]
    fn eval_cond_truthiness() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "t".into(),
            NodeOutput {
                content: json!("something"),
                ..Default::default()
            },
        );
        assert!(eval_cond("{{input}}", "t", &outputs, &BTreeMap::new()));
    }

    #[test]
    fn eval_cond_empty_is_falsy() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "t".into(),
            NodeOutput {
                content: json!(""),
                ..Default::default()
            },
        );
        assert!(!eval_cond("{{input}}", "t", &outputs, &BTreeMap::new()));
    }

    // ---- Task 4: engine sums baseline + computes saved ----------------------

    /// sequential run with non-zero baseline: assert baseline_cost_usd sums
    /// both nodes' baselines, and saved_usd = (baseline - cost).max(0.0).
    #[tokio::test]
    async fn test_sequential_run_baseline_and_saved() {
        let def = make_sequential_def();
        let stub = StubExecutor::new(vec![
            (
                "m1",
                NodeOutput {
                    content: json!("r1"),
                    cost_usd: 0.10,
                    baseline_cost_usd: 0.20,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("r2"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.15,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
        ]);

        let result = run_workflow(
            &stub,
            &def,
            &json!("hi"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        // cost: 0.10 + 0.05 = 0.15
        assert!(
            (result.cost_usd - 0.15).abs() < 1e-9,
            "cost_usd expected 0.15, got {}",
            result.cost_usd
        );
        // baseline: 0.20 + 0.15 = 0.35
        assert!(
            (result.baseline_cost_usd - 0.35).abs() < 1e-9,
            "baseline_cost_usd expected 0.35, got {}",
            result.baseline_cost_usd
        );
        // saved: (0.35 - 0.15).max(0.0) = 0.20
        assert!(
            (result.saved_usd - 0.20).abs() < 1e-9,
            "saved_usd expected 0.20, got {}",
            result.saved_usd
        );
    }

    /// When cost >= baseline, saved_usd must be 0.0 (no negative savings).
    #[tokio::test]
    async fn test_saved_usd_never_negative() {
        let def = make_sequential_def();
        // cost > baseline (pathological but must not produce negative saved)
        let stub = StubExecutor::new(vec![
            (
                "m1",
                NodeOutput {
                    content: json!("r1"),
                    cost_usd: 0.30,
                    baseline_cost_usd: 0.10,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("r2"),
                    cost_usd: 0.0,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
        ]);

        let result = run_workflow(
            &stub,
            &def,
            &json!("x"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        assert_eq!(result.saved_usd, 0.0, "saved_usd must not go negative");
    }

    // ---- Task 2: optional event sink ----------------------------------------

    /// Event sink emits NodeStart + NodeDone for every executed node (including
    /// Trigger and Output which are not journaled) and exactly one terminal
    /// RunDone whose cost_usd / saved_usd match the WorkflowRunResult.
    #[tokio::test]
    async fn run_workflow_emits_node_and_run_events() {
        // Sequential def: t → m1 → m2 → o (4 reachable nodes)
        let def = make_sequential_def();
        let stub = StubExecutor::new(vec![
            (
                "m1",
                NodeOutput {
                    content: json!("r1"),
                    cost_usd: 0.10,
                    baseline_cost_usd: 0.20,
                    model_used: Some("haiku".into()),
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("r2"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.15,
                    model_used: Some("haiku".into()),
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
        ]);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WfEvent>();
        let result = run_workflow(
            &stub,
            &def,
            &json!("hi"),
            None,
            |_| {},
            Some(&tx),
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;
        drop(tx); // close the channel so try_recv returns Disconnected when drained

        assert_eq!(result.status, WfStatus::Succeeded);

        // Drain all events.
        let mut events: Vec<WfEvent> = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }

        // Expect: NodeStart(t), NodeDone(t), NodeStart(m1), NodeDone(m1),
        //         NodeStart(m2), NodeDone(m2), NodeStart(o), NodeDone(o), RunDone
        assert_eq!(
            events.len(),
            9,
            "expected 9 events (4 nodes × 2 + RunDone); got {events:?}"
        );

        // Pair each NodeStart with the following NodeDone for expected node_ids.
        let node_order = ["t", "m1", "m2", "o"];
        for (i, expected_id) in node_order.iter().enumerate() {
            let start_idx = i * 2;
            let done_idx = start_idx + 1;
            match &events[start_idx] {
                WfEvent::NodeStart { node_id } => {
                    assert_eq!(node_id, expected_id, "NodeStart[{i}] wrong node_id")
                }
                other => panic!("events[{start_idx}] expected NodeStart, got {other:?}"),
            }
            match &events[done_idx] {
                WfEvent::NodeDone { node_id, .. } => {
                    assert_eq!(node_id, expected_id, "NodeDone[{i}] wrong node_id")
                }
                other => panic!("events[{done_idx}] expected NodeDone, got {other:?}"),
            }
        }

        // Final event must be RunDone matching WorkflowRunResult.
        match &events[8] {
            WfEvent::RunDone {
                status,
                cost_usd,
                baseline_cost_usd,
                saved_usd,
            } => {
                assert_eq!(status, "completed");
                assert!(
                    (cost_usd - result.cost_usd).abs() < 1e-9,
                    "RunDone cost_usd {cost_usd} != result {}",
                    result.cost_usd
                );
                assert!(
                    (baseline_cost_usd - result.baseline_cost_usd).abs() < 1e-9,
                    "RunDone baseline_cost_usd mismatch"
                );
                assert!(
                    (saved_usd - result.saved_usd).abs() < 1e-9,
                    "RunDone saved_usd {saved_usd} != result {}",
                    result.saved_usd
                );
            }
            other => panic!("events[8] expected RunDone, got {other:?}"),
        }

        // Verify burndown in NodeDone(m1): run_cost=0.10, saved_so_far=0.10 (0.20-0.10).
        if let WfEvent::NodeDone {
            run_cost_usd,
            saved_usd_so_far,
            ..
        } = &events[3]
        {
            assert!(
                (run_cost_usd - 0.10).abs() < 1e-9,
                "m1 NodeDone run_cost_usd expected 0.10, got {run_cost_usd}"
            );
            assert!(
                (saved_usd_so_far - 0.10).abs() < 1e-9,
                "m1 NodeDone saved_usd_so_far expected 0.10, got {saved_usd_so_far}"
            );
        } else {
            panic!("events[3] not NodeDone");
        }
    }

    /// With events=None the returned WorkflowRunResult is byte-identical to
    /// the Some(&tx) run — the event sink does not affect the sync result path.
    #[tokio::test]
    async fn run_workflow_none_events_is_sync_identical() {
        let def = make_sequential_def();
        let responses = vec![
            (
                "m1",
                NodeOutput {
                    content: json!("r1"),
                    cost_usd: 0.10,
                    baseline_cost_usd: 0.20,
                    model_used: Some("haiku".into()),
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("r2"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.15,
                    model_used: Some("haiku".into()),
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
        ];

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<WfEvent>();
        let result_with_events = run_workflow(
            &StubExecutor::new(responses.clone()),
            &def,
            &json!("hi"),
            None,
            |_| {},
            Some(&tx),
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;
        drop(tx);

        let result_none = run_workflow(
            &StubExecutor::new(responses),
            &def,
            &json!("hi"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(
            result_with_events.status, result_none.status,
            "status must be identical"
        );
        assert!(
            (result_with_events.cost_usd - result_none.cost_usd).abs() < 1e-9,
            "cost_usd must be identical"
        );
        assert!(
            (result_with_events.baseline_cost_usd - result_none.baseline_cost_usd).abs() < 1e-9,
            "baseline_cost_usd must be identical"
        );
        assert!(
            (result_with_events.saved_usd - result_none.saved_usd).abs() < 1e-9,
            "saved_usd must be identical"
        );
        assert_eq!(
            result_with_events.error, result_none.error,
            "error must be identical"
        );
    }

    // ---- Wavefront parity tests (W3a-1 Task 1) --------------------------------

    /// Diamond: t → {mb, mc} → out  (trigger fans out to two parallel model
    /// nodes, both converge on a single output node).
    fn make_diamond_def() -> WorkflowDefinition {
        WorkflowDefinition {
            triggers: vec![],
            id: Uuid::nil(),
            version: 1,
            name: "diamond".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "mb".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub-model".into(),
                        },
                        prompt: "{{input}}".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "mc".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub-model".into(),
                        },
                        prompt: "{{input}}".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "out".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "mb".into(),
                    map: None,
                },
                Edge {
                    from: "t".into(),
                    to: "mc".into(),
                    map: None,
                },
                Edge {
                    from: "mb".into(),
                    to: "out".into(),
                    map: None,
                },
                Edge {
                    from: "mc".into(),
                    to: "out".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        }
    }

    /// Parity: 4-node chain (trigger → m1 → m2 → output) must produce the
    /// same WorkflowRunResult as the former linear topo pass.
    #[tokio::test]
    async fn wavefront_linear_chain_matches_expected() {
        let def = make_sequential_def();
        let stub = StubExecutor::new(vec![
            (
                "m1",
                NodeOutput {
                    content: json!("r1"),
                    cost_usd: 0.10,
                    baseline_cost_usd: 0.20,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("r2"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.15,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
        ]);

        let result = run_workflow(
            &stub,
            &def,
            &json!("hi"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        assert!(
            (result.cost_usd - 0.15).abs() < 1e-9,
            "cost_usd expected 0.15, got {}",
            result.cost_usd
        );
        assert!(
            (result.baseline_cost_usd - 0.35).abs() < 1e-9,
            "baseline_cost_usd expected 0.35, got {}",
            result.baseline_cost_usd
        );
        assert!(
            (result.saved_usd - 0.20).abs() < 1e-9,
            "saved_usd expected 0.20, got {}",
            result.saved_usd
        );
        assert_eq!(
            stub.called_nodes(),
            vec!["m1", "m2"],
            "m1 then m2 in topo order"
        );
        // Output node collects m2's result.
        assert_eq!(result.node_outputs.len(), 1, "one output collected");
        assert_eq!(result.node_outputs[0].0, "o");
        assert_eq!(result.node_outputs[0].1.content, json!("r2"));
    }

    /// Diamond (t → {mb, mc} → out): all four nodes must execute; mb before mc
    /// (stable topo order); running three times yields identical results.
    #[tokio::test]
    async fn wavefront_diamond_executes_all_and_is_deterministic() {
        let def = make_diamond_def();

        let make_stub = || {
            StubExecutor::new(vec![
                (
                    "mb",
                    NodeOutput {
                        content: json!("mb_out"),
                        cost_usd: 0.05,
                        baseline_cost_usd: 0.10,
                        model_used: None,
                        doc_vision_saved_est_usd: 0.0,
                    },
                ),
                (
                    "mc",
                    NodeOutput {
                        content: json!("mc_out"),
                        cost_usd: 0.07,
                        baseline_cost_usd: 0.12,
                        model_used: None,
                        doc_vision_saved_est_usd: 0.0,
                    },
                ),
            ])
        };

        // First run: verify both models called in topo order, output collected.
        let stub1 = make_stub();
        let r1 = run_workflow(
            &stub1,
            &def,
            &json!("go"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;
        assert_eq!(r1.status, WfStatus::Succeeded);
        let called = stub1.called_nodes();
        assert_eq!(called, vec!["mb", "mc"], "mb before mc (stable topo order)");
        // Output node has two incoming edges (mb→out, mc→out); both collected.
        assert_eq!(
            r1.node_outputs.len(),
            2,
            "output node collects from both branches"
        );
        assert!(
            (r1.cost_usd - 0.12).abs() < 1e-9,
            "cost_usd expected 0.12, got {}",
            r1.cost_usd
        );

        // Three runs must yield byte-identical results.
        let mut costs: Vec<f64> = Vec::new();
        for _ in 0..3 {
            let r = run_workflow(
                &make_stub(),
                &def,
                &json!("go"),
                None,
                |_| {},
                None,
                &HashMap::new(),
                0,
                &[],
                &NoCache,
            )
            .await;
            assert_eq!(r.status, WfStatus::Succeeded);
            costs.push(r.cost_usd);
        }
        assert!(
            costs.windows(2).all(|w| (w[0] - w[1]).abs() < 1e-9),
            "results must be identical across runs: {costs:?}"
        );
    }

    /// Skipped branch arm + merge node: Branch takes `m_yes`; `m_no` is
    /// unreachable.  The merge output node (`o`) has incoming edges from BOTH
    /// arms but must still execute, because its only *reachable* predecessor
    /// (`m_yes`) is done — wavefront readiness = reachable-preds, not raw
    /// in-degree.
    #[tokio::test]
    async fn wavefront_skipped_branch_merge_still_reachable() {
        let def = make_branch_def(); // t → br(cond={{input}}=="yes") → {m_yes, m_no} → o
        let stub = StubExecutor::new(vec![
            (
                "m_yes",
                NodeOutput {
                    content: json!("yes_out"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
            (
                "m_no",
                NodeOutput {
                    content: json!("no_out"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
        ]);

        // Input "yes" → branch takes m_yes arm; m_no is skipped.
        let result = run_workflow(
            &stub,
            &def,
            &json!("yes"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        let called = stub.called_nodes();
        assert!(
            called.contains(&"m_yes".to_string()),
            "m_yes must run; called: {called:?}"
        );
        assert!(
            !called.contains(&"m_no".to_string()),
            "m_no must NOT run; called: {called:?}"
        );
        // The merge output node must have executed and collected m_yes's result.
        assert_eq!(
            result.node_outputs.len(),
            1,
            "merge output node must execute (reachable-preds readiness)"
        );
        assert_eq!(result.node_outputs[0].0, "o");
        assert_eq!(result.node_outputs[0].1.content, json!("yes_out"));
    }

    // ---- W3a-1 Task 2: concurrent wave tests -----------------------------------

    /// Proof-of-parallelism: mb and mc in the diamond must run concurrently.
    ///
    /// A stub that increments an AtomicUsize on entry, records the peak
    /// in-flight count via fetch_max, then yields before decrementing lets us
    /// observe overlap: when join_all polls mb (yield → pending), it then polls
    /// mc, bringing in_flight to 2 before either completes.
    #[tokio::test]
    async fn concurrent_wave_runs_nodes_in_parallel() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct ParallelProbe {
            in_flight: Arc<AtomicUsize>,
            max_in_flight: Arc<AtomicUsize>,
            responses: HashMap<String, NodeOutput>,
        }

        #[async_trait]
        impl NodeExecutor for ParallelProbe {
            async fn run_intelligence(
                &self,
                node_id: &str,
                _spec: &IntelligenceSpec,
            ) -> Result<NodeOutput, ApiError> {
                // Increment in-flight and record the peak BEFORE yielding so
                // that both mb and mc have incremented before either returns.
                let prev = self.in_flight.fetch_add(1, Ordering::SeqCst);
                self.max_in_flight.fetch_max(prev + 1, Ordering::SeqCst);
                // Yield control so the runtime can poll the other futures in
                // the wave; on the second poll we decrement and return.
                tokio::task::yield_now().await;
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                self.responses
                    .get(node_id)
                    .cloned()
                    .ok_or_else(|| ApiError::Internal(format!("no response for {node_id}")))
            }

            async fn load_subworkflow(&self, id: Uuid) -> Result<WorkflowDefinition, ApiError> {
                Err(ApiError::NotFound(format!(
                    "ParallelProbe stub: no subworkflow {id}"
                )))
            }
        }

        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));

        let stub = ParallelProbe {
            in_flight: Arc::clone(&in_flight),
            max_in_flight: Arc::clone(&max_in_flight),
            responses: [
                (
                    "mb".to_string(),
                    NodeOutput {
                        content: json!("b"),
                        cost_usd: 0.05,
                        baseline_cost_usd: 0.0,
                        model_used: None,
                        doc_vision_saved_est_usd: 0.0,
                    },
                ),
                (
                    "mc".to_string(),
                    NodeOutput {
                        content: json!("c"),
                        cost_usd: 0.05,
                        baseline_cost_usd: 0.0,
                        model_used: None,
                        doc_vision_saved_est_usd: 0.0,
                    },
                ),
            ]
            .into_iter()
            .collect(),
        };

        let def = make_diamond_def(); // t → {mb, mc} → out
        let result = run_workflow(
            &stub,
            &def,
            &json!("go"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        let peak = max_in_flight.load(Ordering::SeqCst);
        assert!(
            peak >= 2,
            "mb and mc must overlap in-flight (max_in_flight = {peak})"
        );
    }

    /// A capped wave reserves and settles nodes one at a time. The first node's
    /// realized cost leaves less than the second node's priceable reservation,
    /// so the sibling is never launched. Before reservation both siblings ran
    /// concurrently and the wave could settle above the shared cap.
    #[tokio::test]
    async fn capped_wave_settles_before_reserving_the_next_sibling() {
        let mut def = make_diamond_def(); // t → {mb, mc} → out
        for node in &mut def.nodes {
            if let NodeKind::Model {
                selection,
                max_output_tokens,
                ..
            } = &mut node.kind
            {
                *selection = ModelSelection::Model {
                    model: "gpt-4o-mini".into(),
                };
                *max_output_tokens = Some(64);
            }
        }
        let stub = StubExecutor::new(vec![
            (
                "mb",
                NodeOutput {
                    content: json!("b"),
                    cost_usd: 0.499_999_9,
                    baseline_cost_usd: 0.5,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
            (
                "mc",
                NodeOutput {
                    content: json!("c"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.05,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
        ]);

        let result = run_workflow(
            &stub,
            &def,
            &json!("go"),
            Some(0.5),
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::BudgetExhausted);
        assert!((result.cost_usd - 0.499_999_9).abs() < 1e-12);
        assert_eq!(
            stub.called_nodes(),
            vec!["mb"],
            "mc must not launch after mb settlement leaves too little for its reservation"
        );
    }

    #[tokio::test]
    async fn capped_wave_passes_each_node_the_actual_remaining_run_value() {
        let mut def = make_diamond_def();
        for node in &mut def.nodes {
            if let NodeKind::Model {
                selection,
                max_output_tokens,
                ..
            } = &mut node.kind
            {
                *selection = ModelSelection::Model {
                    model: "gpt-4o-mini".into(),
                };
                *max_output_tokens = Some(64);
            }
        }
        let stub = StubExecutor::new(vec![
            (
                "mb",
                NodeOutput {
                    content: json!("b"),
                    cost_usd: 0.25,
                    baseline_cost_usd: 0.25,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
            (
                "mc",
                NodeOutput {
                    content: json!("c"),
                    cost_usd: 0.10,
                    baseline_cost_usd: 0.10,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
        ]);

        let result = run_workflow(
            &stub,
            &def,
            &json!("go"),
            Some(0.5),
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        assert_eq!(stub.called_nodes(), vec!["mb", "mc"]);
        assert_eq!(
            stub.cost_caps(),
            vec![
                ("mb".to_string(), Some(0.5)),
                ("mc".to_string(), Some(0.25)),
            ],
        );
    }

    #[tokio::test]
    async fn capped_wave_does_not_launch_a_node_whose_own_cap_cannot_fit_it() {
        let mut def = make_sequential_def();
        for node in &mut def.nodes {
            if let NodeKind::Model {
                selection,
                max_output_tokens,
                max_cost_usd,
                ..
            } = &mut node.kind
            {
                *selection = ModelSelection::Model {
                    model: "gpt-4o-mini".into(),
                };
                *max_output_tokens = Some(64);
                *max_cost_usd = Some(0.000_000_001);
            }
        }
        let stub = StubExecutor::new(vec![]);

        let result = run_workflow(
            &stub,
            &def,
            &json!("go"),
            Some(1.0),
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::BudgetExhausted);
        assert!(stub.called_nodes().is_empty());
    }

    /// Hard-budget gate before a capped wave.
    ///
    /// A "prior" node brings accrued to exactly the cap (1.0).  The next wave
    /// contains 3 Model nodes each costing 0.40.  The gate fires BEFORE
    /// launching any of them, so none are called and status is BudgetExhausted.
    ///
    /// # Gate guarantee
    /// No Model/Agent node is ever launched when `accrued >= run_max_cost_usd`.
    #[tokio::test]
    async fn hard_budget_under_concurrency() {
        // Workflow: t → prior → {n1, n2, n3} → out
        let def = WorkflowDefinition {
            triggers: vec![],
            id: Uuid::nil(),
            version: 1,
            name: "budget_gate".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "prior".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model { model: "s".into() },
                        prompt: "p".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "n1".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model { model: "s".into() },
                        prompt: "1".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "n2".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model { model: "s".into() },
                        prompt: "2".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "n3".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model { model: "s".into() },
                        prompt: "3".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "out".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "prior".into(),
                    map: None,
                },
                Edge {
                    from: "prior".into(),
                    to: "n1".into(),
                    map: None,
                },
                Edge {
                    from: "prior".into(),
                    to: "n2".into(),
                    map: None,
                },
                Edge {
                    from: "prior".into(),
                    to: "n3".into(),
                    map: None,
                },
                Edge {
                    from: "n1".into(),
                    to: "out".into(),
                    map: None,
                },
                Edge {
                    from: "n2".into(),
                    to: "out".into(),
                    map: None,
                },
                Edge {
                    from: "n3".into(),
                    to: "out".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        };

        let stub = StubExecutor::new(vec![
            (
                "prior",
                NodeOutput {
                    content: json!("p"),
                    cost_usd: 1.0,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
            (
                "n1",
                NodeOutput {
                    content: json!("n1"),
                    cost_usd: 0.40,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
            (
                "n2",
                NodeOutput {
                    content: json!("n2"),
                    cost_usd: 0.40,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
            (
                "n3",
                NodeOutput {
                    content: json!("n3"),
                    cost_usd: 0.40,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
        ]);

        // cap = 1.0.  prior: accrued(0.0) < 1.0 → launches, costs 1.0 → accrued=1.0.
        // Wave {n1,n2,n3}: budget_reached(1.0, Some(1.0)) = true → NONE launched.
        let result = run_workflow(
            &stub,
            &def,
            &json!("x"),
            Some(1.0),
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::BudgetExhausted);
        assert!(
            (result.cost_usd - 1.0).abs() < 1e-9,
            "only prior's cost must be accrued; got {}",
            result.cost_usd
        );
        let called = stub.called_nodes();
        assert_eq!(
            called,
            vec!["prior"],
            "n1/n2/n3 must NOT be launched (hard budget gate fired); called: {called:?}"
        );
    }

    /// Determinism: running the diamond 5× must yield identical results.
    ///
    /// The post-join fold in stable topo-index order guarantees determinism
    /// regardless of which concurrent future completes first.
    #[tokio::test]
    async fn concurrent_result_is_deterministic() {
        let make_stub = || {
            StubExecutor::new(vec![
                (
                    "mb",
                    NodeOutput {
                        content: json!("b"),
                        cost_usd: 0.05,
                        baseline_cost_usd: 0.10,
                        model_used: None,
                        doc_vision_saved_est_usd: 0.0,
                    },
                ),
                (
                    "mc",
                    NodeOutput {
                        content: json!("c"),
                        cost_usd: 0.07,
                        baseline_cost_usd: 0.12,
                        model_used: None,
                        doc_vision_saved_est_usd: 0.0,
                    },
                ),
            ])
        };

        let def = make_diamond_def();
        let mut results = Vec::new();
        for _ in 0..5 {
            let r = run_workflow(
                &make_stub(),
                &def,
                &json!("go"),
                None,
                |_| {},
                None,
                &HashMap::new(),
                0,
                &[],
                &NoCache,
            )
            .await;
            results.push(r);
        }

        let first = &results[0];
        assert_eq!(first.status, WfStatus::Succeeded);
        for r in &results[1..] {
            assert_eq!(
                r.status, first.status,
                "status must be identical across runs"
            );
            assert!(
                (r.cost_usd - first.cost_usd).abs() < 1e-9,
                "cost_usd must be identical across runs"
            );
            assert!(
                (r.baseline_cost_usd - first.baseline_cost_usd).abs() < 1e-9,
                "baseline_cost_usd must be identical across runs"
            );
            assert!(
                (r.saved_usd - first.saved_usd).abs() < 1e-9,
                "saved_usd must be identical across runs"
            );
            assert_eq!(
                r.node_outputs.len(),
                first.node_outputs.len(),
                "node_output count must match"
            );
        }
    }

    // ---- W3a-1 review: Fix #1 + Fix #4 guard tests ----------------------------

    /// Guards Fix #1 (drain-all cost on partial wave failure).
    ///
    /// In the diamond (t → {mb, mc} → out), mb is first in topo order and
    /// errors (no stub response); mc succeeds with cost 0.15.  Before Fix #1
    /// the fold returns on mb's error, so mc's cost is never accrued →
    /// cost_usd == 0.0, not 0.15.  After Fix #1 the fold drains the entire
    /// wave before bailing: mc accrues, result is Failed with cost_usd 0.15.
    #[tokio::test]
    async fn partial_wave_failure_reports_full_cost() {
        let def = make_diamond_def(); // t → {mb, mc} → out
                                      // mb has no stub response → Err; mc succeeds.
        let stub = StubExecutor::new(vec![(
            "mc",
            NodeOutput {
                content: json!("c"),
                cost_usd: 0.15,
                baseline_cost_usd: 0.30,
                model_used: None,
                doc_vision_saved_est_usd: 0.0,
            },
        )]);

        let result = run_workflow(
            &stub,
            &def,
            &json!("go"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Failed, "run must be Failed");
        assert!(
            (result.cost_usd - 0.15).abs() < 1e-9,
            "cost_usd must include mc's cost (0.15) even though mb failed first; got {}",
            result.cost_usd
        );
        assert!(
            (result.baseline_cost_usd - 0.30).abs() < 1e-9,
            "baseline_cost_usd must include mc's baseline (0.30); got {}",
            result.baseline_cost_usd
        );
        assert!(result.error.is_some(), "error field must be set");
        assert!(
            result.error.as_deref().unwrap().contains("mb"),
            "error must name the failing node (mb); got {:?}",
            result.error
        );
    }

    /// Guards Fix #4 (NodeStart emitted before concurrent launch).
    ///
    /// In the diamond (t → {mb, mc} → out), all wave-node NodeStart events
    /// must appear before any NodeDone for those nodes.  Before Fix #4 the
    /// fold emits NodeStart then NodeDone for each node in sequence; after
    /// Fix #4 all NodeStarts are pre-emitted and only NodeDones appear in
    /// the fold → all starts precede all dones.
    #[tokio::test]
    async fn concurrent_nodes_emit_nodestart_before_completion() {
        let def = make_diamond_def();
        let stub = StubExecutor::new(vec![
            (
                "mb",
                NodeOutput {
                    content: json!("b"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
            (
                "mc",
                NodeOutput {
                    content: json!("c"),
                    cost_usd: 0.07,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
        ]);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WfEvent>();
        let result = run_workflow(
            &stub,
            &def,
            &json!("go"),
            None,
            |_| {},
            Some(&tx),
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;
        drop(tx);
        assert_eq!(result.status, WfStatus::Succeeded);

        let mut events: Vec<WfEvent> = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }

        // Keep only NodeStart / NodeDone events for the concurrent wave nodes.
        let wave_events: Vec<&WfEvent> = events
            .iter()
            .filter(|ev| match ev {
                WfEvent::NodeStart { node_id } => node_id == "mb" || node_id == "mc",
                WfEvent::NodeDone { node_id, .. } => node_id == "mb" || node_id == "mc",
                _ => false,
            })
            .collect();

        assert_eq!(
            wave_events.len(),
            4,
            "expected 4 wave events (2×NodeStart + 2×NodeDone); got {wave_events:?}"
        );

        let start_positions: Vec<usize> = wave_events
            .iter()
            .enumerate()
            .filter(|(_, ev)| matches!(ev, WfEvent::NodeStart { .. }))
            .map(|(i, _)| i)
            .collect();
        let done_positions: Vec<usize> = wave_events
            .iter()
            .enumerate()
            .filter(|(_, ev)| matches!(ev, WfEvent::NodeDone { .. }))
            .map(|(i, _)| i)
            .collect();

        assert_eq!(start_positions.len(), 2, "must have 2 NodeStart events");
        assert_eq!(done_positions.len(), 2, "must have 2 NodeDone events");

        let last_start = *start_positions.iter().max().unwrap();
        let first_done = *done_positions.iter().min().unwrap();

        assert!(
            last_start < first_done,
            "all wave NodeStart events must precede all wave NodeDone events; \
             starts={start_positions:?} dones={done_positions:?}"
        );
    }

    /// Regression: a purely sequential chain (waves of size 1) must produce
    /// the same result as the former sequential implementation — no regression
    /// from the concurrent-wave refactor.
    #[tokio::test]
    async fn concurrent_linear_chain_parity() {
        let def = make_sequential_def();
        let stub = StubExecutor::new(vec![
            (
                "m1",
                NodeOutput {
                    content: json!("r1"),
                    cost_usd: 0.10,
                    baseline_cost_usd: 0.20,
                    model_used: Some("haiku".into()),
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("r2"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.15,
                    model_used: Some("haiku".into()),
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
        ]);

        let result = run_workflow(
            &stub,
            &def,
            &json!("hi"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        assert!(
            (result.cost_usd - 0.15).abs() < 1e-9,
            "cost_usd expected 0.15, got {}",
            result.cost_usd
        );
        assert!(
            (result.baseline_cost_usd - 0.35).abs() < 1e-9,
            "baseline_cost_usd expected 0.35, got {}",
            result.baseline_cost_usd
        );
        assert!(
            (result.saved_usd - 0.20).abs() < 1e-9,
            "saved_usd expected 0.20, got {}",
            result.saved_usd
        );
        assert_eq!(
            stub.called_nodes(),
            vec!["m1", "m2"],
            "m1 then m2 in sequential (topo) order"
        );
    }

    // ---- W3b Task 3: security guard tests -----------------------------------

    /// Verifies that the shared `substitute`/`resolve_ref` path NEVER returns a
    /// real secret value for `{{secrets.*}}` refs.  This ensures Model/Agent
    /// prompts, Transform exprs, and Branch conditions are always secret-free.
    #[test]
    fn shared_substitute_is_secret_free() {
        let outputs = HashMap::new();

        // `{{secrets.K}}` → "***" (explicit redaction marker, not the secret).
        let result = substitute("text {{secrets.K}} more", "t", &outputs, &BTreeMap::new());
        assert_eq!(
            result, "text *** more",
            "shared substitute must redact secrets.* refs; got: {result}"
        );

        // `{{secrets}}` (no dot) → "***".
        let result2 = substitute("{{secrets}}", "t", &outputs, &BTreeMap::new());
        assert_eq!(
            result2, "***",
            "shared substitute must redact bare {{secrets}}; got: {result2}"
        );

        // Confirm it is NOT the real secret value (belt-and-suspenders).
        assert_ne!(result, "sekret-value");
        assert_ne!(result2, "sekret-value");
    }

    /// THE REDACTION GUARD: asserts that a secret value NEVER appears in any
    /// observable output field after running a workflow with an Http node whose
    /// header references `{{secrets.K}}`.
    ///
    /// The Http node uses `allowed_hosts: ["other-host.com"]` but the URL host
    /// is `api.example.com` → `HostNotAllowed` fires immediately (no network
    /// call), giving us a deterministic pure-unit test.
    #[tokio::test]
    async fn http_node_never_journals_secret() {
        let secret_value = "sekret-value";
        let mut secrets = HashMap::new();
        secrets.insert("K".to_string(), SecretString::new(secret_value.to_string()));

        // Workflow: t → h (Http node referencing secret in header).
        let def = WorkflowDefinition {
            triggers: vec![],
            id: Uuid::nil(),
            version: 1,
            name: "redaction_test".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "h".into(),
                    kind: NodeKind::Http {
                        method: "GET".into(),
                        url: "https://api.example.com/x".into(),
                        // x-auth-token is not in DENIED_HEADERS so it passes filter_extra_headers.
                        headers: vec![("x-auth-token".into(), "Bearer {{secrets.K}}".into())],
                        body: None,
                        max_response_bytes: None,
                    },
                },
            ],
            edges: vec![Edge {
                from: "t".into(),
                to: "h".into(),
                map: None,
            }],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            // "other-host.com" is in allowed_hosts, but URL host is "api.example.com"
            // → HostNotAllowed fires immediately without any network call.
            allowed_hosts: vec!["other-host.com".to_string()],
            metadata: serde_json::Value::Null,
        };

        let stub = StubExecutor::new(vec![]);
        let mut journal_entries: Vec<NodeJournalEntry> = Vec::new();
        let result = run_workflow(
            &stub,
            &def,
            &json!("input"),
            None,
            |e| journal_entries.push(e),
            None,
            &secrets,
            0,
            &[],
            &NoCache,
        )
        .await;

        // The run must have failed (Http node got HostNotAllowed).
        assert_eq!(
            result.status,
            WfStatus::Failed,
            "run should fail due to HostNotAllowed"
        );
        assert_eq!(journal_entries.len(), 1);
        assert_eq!(journal_entries[0].node_id, "h");
        assert_eq!(journal_entries[0].status, "failed");
        assert!(journal_entries[0].finished_at >= journal_entries[0].started_at);

        // ---- SECURITY INVARIANT: "sekret-value" must NOT appear anywhere ----

        // 1. result.error must not contain the secret.
        if let Some(ref err_str) = result.error {
            assert!(
                !err_str.contains(secret_value),
                "secret leaked into result.error: {err_str}"
            );
        }

        // 2. Journal entries (output + error fields) must not contain the secret.
        for entry in &journal_entries {
            let output_str = format!("{:?}", entry.output);
            assert!(
                !output_str.contains(secret_value),
                "secret leaked into journal output for {}: {output_str}",
                entry.node_id
            );
            if let Some(ref entry_err) = entry.error {
                assert!(
                    !entry_err.contains(secret_value),
                    "secret leaked into journal error for {}: {entry_err}",
                    entry.node_id
                );
            }
        }

        // 3. Collected node outputs must not contain the secret.
        for (nid, out) in &result.node_outputs {
            let content_str = format!("{:?}", out.content);
            assert!(
                !content_str.contains(secret_value),
                "secret leaked into node_outputs[{nid}]: {content_str}"
            );
        }
    }

    /// Missing Http secrets fail before the Trigger or Http node is executed.
    /// The unavailable name may be reported as safe metadata, but no wire call
    /// or node journal entry is allowed.
    #[tokio::test]
    async fn missing_http_secret_fails_before_any_node() {
        let def = WorkflowDefinition {
            triggers: vec![],
            id: Uuid::nil(),
            version: 1,
            name: "missing_secret".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "h".into(),
                    kind: NodeKind::Http {
                        method: "GET".into(),
                        url: "https://api.example.com/x".into(),
                        headers: vec![(
                            "authorization".into(),
                            "Bearer {{secrets.MISSING_KEY}}".into(),
                        )],
                        body: None,
                        max_response_bytes: None,
                    },
                },
            ],
            edges: vec![Edge {
                from: "t".into(),
                to: "h".into(),
                map: None,
            }],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec!["other-host.com".into()],
            metadata: serde_json::Value::Null,
        };

        let stub = StubExecutor::new(vec![]);
        let mut journal_entries = Vec::new();
        let result = run_workflow(
            &stub,
            &def,
            &json!("input"),
            None,
            |entry| journal_entries.push(entry),
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Failed);
        assert_eq!(result.cost_usd, 0.0);
        assert!(journal_entries.is_empty());
        assert!(stub.called_nodes().is_empty());
        assert!(result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("MISSING_KEY")));
    }

    // ---- Task 1 (W3a-3): StubExecutor::load_subworkflow registry ----------------

    /// Registering a WorkflowDefinition in StubExecutor and calling
    /// `load_subworkflow` returns a clone of it; an unregistered id returns
    /// `Err(ApiError::NotFound)`.
    #[tokio::test]
    async fn stub_load_subworkflow_returns_registered_def() {
        let child_id = Uuid::new_v4();
        let child_def = WorkflowDefinition {
            triggers: vec![],
            id: child_id,
            version: 1,
            name: "child-wf".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![Edge {
                from: "t".into(),
                to: "o".into(),
                map: None,
            }],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        };

        let mut stub = StubExecutor::new(vec![]);
        stub.subworkflows.insert(child_id, child_def.clone());

        // Registered id → Ok with the same definition.
        let result = stub.load_subworkflow(child_id).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        let loaded = result.unwrap();
        assert_eq!(loaded.id, child_id);
        assert_eq!(loaded.name, "child-wf");

        // Unregistered id → Err(NotFound).
        let unknown_id = Uuid::new_v4();
        let miss = stub.load_subworkflow(unknown_id).await;
        assert!(
            matches!(miss, Err(ApiError::NotFound(_))),
            "expected NotFound for unknown id, got {miss:?}"
        );
    }

    // ---- W3a-3 Task 2: SubWorkflow node + cost rollup -------------------------

    /// A parent workflow with a SubWorkflow node pointing at a child def;
    /// the child runs one Model node costing 0.05 (baseline 0.10).
    /// Assert: parent status Succeeded, cost_usd == 0.05 (child's model cost),
    /// baseline_cost_usd == 0.10, saved_usd == 0.05, node_outputs contains the
    /// SubWorkflow node's content (serialized child node_outputs).
    #[tokio::test]
    async fn subworkflow_runs_child_and_rolls_up_cost() {
        let child_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();

        // Child: t → m1 (costs 0.05 / baseline 0.10) → o
        let child_def = WorkflowDefinition {
            triggers: vec![],
            id: child_id,
            version: 1,
            name: "child-wf".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "m1".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub".into(),
                        },
                        prompt: "{{input}}".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "m1".into(),
                    map: None,
                },
                Edge {
                    from: "m1".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        };

        // Parent: t → sw (SubWorkflow → child_id) → o
        let parent_def = WorkflowDefinition {
            triggers: vec![],
            id: parent_id,
            version: 1,
            name: "parent-wf".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "sw".into(),
                    kind: NodeKind::SubWorkflow {
                        workflow_id: child_id,
                        version: None,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "sw".into(),
                    map: None,
                },
                Edge {
                    from: "sw".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        };

        let mut stub = StubExecutor::new(vec![(
            "m1",
            NodeOutput {
                content: json!("child_response"),
                cost_usd: 0.05,
                baseline_cost_usd: 0.10,
                model_used: Some("haiku".into()),
                doc_vision_saved_est_usd: 0.0,
            },
        )]);
        stub.subworkflows.insert(child_id, child_def);

        let result = run_workflow(
            &stub,
            &parent_def,
            &json!("hello"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        // Status: Succeeded
        assert_eq!(result.status, WfStatus::Succeeded, "parent must Succeed");
        // Cost rollup: child's model cost 0.05
        assert!(
            (result.cost_usd - 0.05).abs() < 1e-9,
            "cost_usd expected 0.05, got {}",
            result.cost_usd
        );
        // Baseline rollup: child's baseline 0.10
        assert!(
            (result.baseline_cost_usd - 0.10).abs() < 1e-9,
            "baseline_cost_usd expected 0.10, got {}",
            result.baseline_cost_usd
        );
        // Saved = baseline - cost = 0.05 (no double-count)
        assert!(
            (result.saved_usd - 0.05).abs() < 1e-9,
            "saved_usd expected 0.05, got {}",
            result.saved_usd
        );
        // Output node collected sw's content (the child's node_outputs serialized)
        assert_eq!(result.node_outputs.len(), 1, "one output node collected");
        assert_eq!(result.node_outputs[0].0, "o");
        assert!(
            !result.node_outputs[0].1.content.is_null(),
            "SubWorkflow output content must not be null"
        );
        // The content is the child's node_outputs serialized as JSON
        let content = &result.node_outputs[0].1.content;
        assert!(
            content.is_array(),
            "SubWorkflow output content must be an array (child node_outputs); got {content}"
        );
    }

    // ---- W3a-3 Task 3: recursion-guard + budget-rollup suite -----------------

    /// Helper: build a leaf WorkflowDefinition (Trigger → Output) with the
    /// given id.  No model nodes, so it costs nothing and always succeeds.
    fn make_leaf_def(id: Uuid) -> WorkflowDefinition {
        WorkflowDefinition {
            triggers: vec![],
            id,
            version: 1,
            name: format!("leaf-{id}"),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![Edge {
                from: "t".into(),
                to: "o".into(),
                map: None,
            }],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        }
    }

    /// Helper: build a workflow with `id` whose single node (after Trigger) is
    /// a SubWorkflow pointing at `child_id`, then an Output node.
    fn make_subwf_def(id: Uuid, child_id: Uuid) -> WorkflowDefinition {
        WorkflowDefinition {
            triggers: vec![],
            id,
            version: 1,
            name: format!("wf-{id}"),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "sw".into(),
                    kind: NodeKind::SubWorkflow {
                        workflow_id: child_id,
                        version: None,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "sw".into(),
                    map: None,
                },
                Edge {
                    from: "sw".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        }
    }

    /// GUARD TEST 1 — Self-cycle: a workflow whose SubWorkflow node points at
    /// its own id must be rejected immediately with status Failed + "cycle" in
    /// the error message.  The test completing proves termination (no hang /
    /// stack overflow).
    #[tokio::test]
    async fn subworkflow_self_cycle_rejected() {
        let wf_id = Uuid::new_v4();
        // W points at itself.
        let def = make_subwf_def(wf_id, wf_id);

        // No child registered — the cycle guard must fire BEFORE load_subworkflow.
        let stub = StubExecutor::new(vec![]);

        let result = run_workflow(
            &stub,
            &def,
            &json!("hello"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(
            result.status,
            WfStatus::Failed,
            "self-cycle must produce Failed; got {:?}",
            result.status
        );
        let err = result
            .error
            .expect("self-cycle must produce an error string");
        assert!(
            err.contains("cycle"),
            "error must mention 'cycle'; got: {err}"
        );
    }

    /// GUARD TEST 2 — Indirect cycle: A → B → A must be rejected with Failed +
    /// "cycle".  The test completing proves termination.
    #[tokio::test]
    async fn subworkflow_indirect_cycle_rejected() {
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();

        // A points at B; B points back at A → indirect cycle.
        let def_a = make_subwf_def(id_a, id_b);
        let def_b = make_subwf_def(id_b, id_a);

        let mut stub = StubExecutor::new(vec![]);
        stub.subworkflows.insert(id_a, def_a.clone());
        stub.subworkflows.insert(id_b, def_b);

        let result = run_workflow(
            &stub,
            &def_a,
            &json!("hello"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(
            result.status,
            WfStatus::Failed,
            "indirect A→B→A cycle must produce Failed; got {:?}",
            result.status
        );
        let err = result
            .error
            .expect("indirect cycle must produce an error string");
        assert!(
            err.contains("cycle"),
            "error must mention 'cycle'; got: {err}"
        );
    }

    /// GUARD TEST 3 — Depth cap: chain A→B→C→D→E→F→G (7 workflows, 6
    /// SubWorkflow hops, each distinct UUID so it's depth — not cycle — that
    /// trips).  MAX_SUBWORKFLOW_DEPTH=5 means the guard fires when a workflow
    /// running at depth=5 tries to call a SubWorkflow (`5 >= 5`).  The test
    /// completing proves termination.
    ///
    /// Depth accounting:
    ///   ids[0] runs at depth 0, calls ids[1]
    ///   ids[1] runs at depth 1, calls ids[2]
    ///   ids[2] runs at depth 2, calls ids[3]
    ///   ids[3] runs at depth 3, calls ids[4]
    ///   ids[4] runs at depth 4, calls ids[5]
    ///   ids[5] runs at depth 5, tries to call ids[6] → guard `5 >= 5` → FAIL
    #[tokio::test]
    async fn subworkflow_depth_cap_rejected() {
        // 7 distinct ids: ids[0..5] are SubWorkflow defs, ids[6] is a leaf.
        let ids: Vec<Uuid> = (0..7).map(|_| Uuid::new_v4()).collect();

        // Build defs: ids[0]→ids[1]→…→ids[5] (SubWorkflow) + ids[6] (leaf)
        let mut defs: Vec<WorkflowDefinition> = Vec::new();
        for i in 0..6 {
            defs.push(make_subwf_def(ids[i], ids[i + 1]));
        }
        defs.push(make_leaf_def(ids[6]));

        let mut stub = StubExecutor::new(vec![]);
        for def in &defs {
            stub.subworkflows.insert(def.id, def.clone());
        }

        // Run starting at A (ids[0]).
        let result = run_workflow(
            &stub,
            &defs[0],
            &json!("hello"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(
            result.status,
            WfStatus::Failed,
            "depth-6 chain must produce Failed; got {:?}",
            result.status
        );
        let err = result
            .error
            .expect("depth cap must produce an error string");
        // The error message is "sub-workflow nesting exceeds max depth 5"
        assert!(
            err.contains("depth") || err.contains("nesting"),
            "error must mention 'depth' or 'nesting'; got: {err}"
        );
    }

    /// ROLLUP TEST — No-double-count: a child costing 0.30 / baseline 0.50
    /// (saved = 0.20) wrapped by a parent whose only real work is the
    /// SubWorkflow node.  After rollup:
    ///   parent.cost_usd       == 0.30
    ///   parent.baseline_cost  == 0.50
    ///   parent.saved_usd      == 0.20   (NOT 0.40 — the no-double-count guard)
    #[tokio::test]
    async fn subworkflow_budget_rolls_up_no_double_count() {
        let child_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();

        // Child: t → m1 (cost=0.30, baseline=0.50) → o
        let child_def = WorkflowDefinition {
            triggers: vec![],
            id: child_id,
            version: 1,
            name: "child-wf".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "m1".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub".into(),
                        },
                        prompt: "{{input}}".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "m1".into(),
                    map: None,
                },
                Edge {
                    from: "m1".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        };

        let parent_def = make_subwf_def(parent_id, child_id);

        let mut stub = StubExecutor::new(vec![(
            "m1",
            NodeOutput {
                content: json!("child_response"),
                cost_usd: 0.30,
                baseline_cost_usd: 0.50,
                model_used: Some("haiku".into()),
                doc_vision_saved_est_usd: 0.0,
            },
        )]);
        stub.subworkflows.insert(child_id, child_def);

        let result = run_workflow(
            &stub,
            &parent_def,
            &json!("input"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded, "parent must Succeed");

        // cost_usd must equal the child's model cost (0.30 — not 0.60)
        assert!(
            (result.cost_usd - 0.30).abs() < 1e-9,
            "cost_usd expected 0.30 (child cost once), got {}",
            result.cost_usd
        );
        // baseline_cost_usd must equal child's baseline (0.50 — not 1.00)
        assert!(
            (result.baseline_cost_usd - 0.50).abs() < 1e-9,
            "baseline_cost_usd expected 0.50 (child baseline once), got {}",
            result.baseline_cost_usd
        );
        // saved_usd == baseline - cost == 0.20  (NOT 0.40 — the double-count would be 0.40)
        assert!(
            (result.saved_usd - 0.20).abs() < 1e-9,
            "saved_usd expected 0.20 (no double-count); got {} — double-count would be 0.40",
            result.saved_usd
        );
    }

    /// OUTPUT MAPPING TEST — the child's node_outputs are serialized into the
    /// SubWorkflow node's `content` in the parent; the content must be a
    /// non-null, non-empty array.
    #[tokio::test]
    async fn subworkflow_output_mapped() {
        let child_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();

        // Child: t → m1 → o
        let child_def = WorkflowDefinition {
            triggers: vec![],
            id: child_id,
            version: 1,
            name: "child-wf".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "m1".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub".into(),
                        },
                        prompt: "{{input}}".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "m1".into(),
                    map: None,
                },
                Edge {
                    from: "m1".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        };

        // Parent: t → sw → o  (output node collects sw's content)
        let parent_def = make_subwf_def(parent_id, child_id);

        let mut stub = StubExecutor::new(vec![(
            "m1",
            NodeOutput {
                content: json!("child_payload"),
                cost_usd: 0.01,
                baseline_cost_usd: 0.02,
                model_used: Some("haiku".into()),
                doc_vision_saved_est_usd: 0.0,
            },
        )]);
        stub.subworkflows.insert(child_id, child_def);

        let result = run_workflow(
            &stub,
            &parent_def,
            &json!("hi"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        // The parent's output node ("o") collects the SubWorkflow node ("sw") content.
        assert_eq!(result.node_outputs.len(), 1);
        assert_eq!(result.node_outputs[0].0, "o");
        let content = &result.node_outputs[0].1.content;
        assert!(
            !content.is_null(),
            "SubWorkflow output content must not be null"
        );
        assert!(
            content.is_array(),
            "SubWorkflow output content must be an array (child node_outputs); got {content}"
        );
        // The array must contain the child's Output node entry.
        let arr = content.as_array().unwrap();
        assert!(
            !arr.is_empty(),
            "SubWorkflow output array must be non-empty; child had an Output node"
        );
        // Each element is a [node_id, NodeOutput] tuple — verify child output is present.
        let has_child_output = arr.iter().any(|entry| {
            entry
                .as_array()
                .and_then(|pair| pair.first())
                .and_then(|v| v.as_str())
                == Some("o")
        });
        assert!(
            has_child_output,
            "child Output node 'o' must appear in the SubWorkflow content; got {content}"
        );
    }

    /// CHILD FAILURE PROPAGATION TEST — a child that fails (stub returns an
    /// error for its model node) causes the parent run to fail too; the parent
    /// status is Failed and an error message is present.
    #[tokio::test]
    async fn subworkflow_child_failure_propagates() {
        let child_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();

        // Child has a Model node "m1" — but the stub has NO response for "m1",
        // so the stub returns ApiError::Internal → child run Fails.
        let child_def = WorkflowDefinition {
            triggers: vec![],
            id: child_id,
            version: 1,
            name: "failing-child".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "m1".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub".into(),
                        },
                        prompt: "{{input}}".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "m1".into(),
                    map: None,
                },
                Edge {
                    from: "m1".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        };

        let parent_def = make_subwf_def(parent_id, child_id);

        // No model responses → stub.run_intelligence("m1", ...) returns Err.
        let mut stub = StubExecutor::new(vec![]);
        stub.subworkflows.insert(child_id, child_def);

        let result = run_workflow(
            &stub,
            &parent_def,
            &json!("input"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(
            result.status,
            WfStatus::Failed,
            "child failure must propagate to parent as Failed; got {:?}",
            result.status
        );
        assert!(
            result.error.is_some(),
            "parent must report an error when the child fails"
        );
    }

    /// BUDGET EXHAUSTION IN CHILD — a child with a model node, run with a
    /// budget so small that the pre-node budget gate fires before any model
    /// call.  The child must return BudgetExhausted, which propagates to the
    /// parent.
    ///
    /// The pre-node gate fires when `accrued >= run_max_cost_usd`.  We set
    /// `run_max_cost_usd = Some(0.0)` on the parent (passed down to the child
    /// as `remaining = 0.0`).  With 0 remaining the child's budget gate fires
    /// before the model node runs → child BudgetExhausted → parent BudgetExhausted.
    #[tokio::test]
    async fn subworkflow_budget_exhaustion_in_child() {
        let child_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();

        // Child has a Model node that would cost 0.10, but we'll exhaust budget before it runs.
        let child_def = WorkflowDefinition {
            triggers: vec![],
            id: child_id,
            version: 1,
            name: "budget-child".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "m1".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub".into(),
                        },
                        prompt: "{{input}}".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "m1".into(),
                    map: None,
                },
                Edge {
                    from: "m1".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        };

        let parent_def = make_subwf_def(parent_id, child_id);

        let mut stub = StubExecutor::new(vec![(
            "m1",
            NodeOutput {
                content: json!("should_not_run"),
                cost_usd: 0.10,
                baseline_cost_usd: 0.20,
                model_used: Some("haiku".into()),
                doc_vision_saved_est_usd: 0.0,
            },
        )]);
        stub.subworkflows.insert(child_id, child_def);

        // Budget of 0.0 — even the first Model node is blocked.
        let result = run_workflow(
            &stub,
            &parent_def,
            &json!("input"),
            Some(0.0),
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(
            result.status,
            WfStatus::BudgetExhausted,
            "child BudgetExhausted must propagate to parent; got {:?}",
            result.status
        );
    }

    // ---- W3a-2 Task 1: Loop node -------------------------------------------

    /// A parent workflow with a Loop node whose body runs a single Model node
    /// costing 0.05 (baseline 0.10).  With max_iters=3 and an always-truthy
    /// cond (`"{{input}}"` with input="hello"), the body runs exactly 3 times.
    /// Assert: status Succeeded, cost_usd==0.15 (3×0.05), baseline==0.30,
    /// saved==0.15, body model called 3 times.
    #[tokio::test]
    async fn loop_runs_body_n_times() {
        let body_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();

        // Body: t → m1 (costs 0.05 / baseline 0.10) → o
        let body_def = WorkflowDefinition {
            triggers: vec![],
            id: body_id,
            version: 1,
            name: "body-wf".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "m1".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub".into(),
                        },
                        prompt: "{{input}}".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "m1".into(),
                    map: None,
                },
                Edge {
                    from: "m1".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        };

        // Parent: t → lp (Loop → body_id, max_iters=3, cond="{{input}}") → o
        // cond "{{input}}" resolves to "hello" (truthy) throughout all iters.
        let parent_def = WorkflowDefinition {
            triggers: vec![],
            id: parent_id,
            version: 1,
            name: "parent-wf".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "lp".into(),
                    kind: NodeKind::Loop {
                        body_workflow_id: body_id,
                        cond: "{{input}}".into(),
                        max_iters: 3,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "lp".into(),
                    map: None,
                },
                Edge {
                    from: "lp".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        };

        let mut stub = StubExecutor::new(vec![(
            "m1",
            NodeOutput {
                content: json!("body_response"),
                cost_usd: 0.05,
                baseline_cost_usd: 0.10,
                model_used: Some("stub-model".into()),
                doc_vision_saved_est_usd: 0.0,
            },
        )]);
        stub.subworkflows.insert(body_id, body_def);

        let result = run_workflow(
            &stub,
            &parent_def,
            &json!("hello"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        // Status: Succeeded
        assert_eq!(
            result.status,
            WfStatus::Succeeded,
            "Loop parent must Succeed"
        );
        // Cost rollup: 3 iterations × 0.05
        assert!(
            (result.cost_usd - 0.15).abs() < 1e-9,
            "cost_usd expected 0.15 (3×0.05), got {}",
            result.cost_usd
        );
        // Baseline rollup: 3 iterations × 0.10
        assert!(
            (result.baseline_cost_usd - 0.30).abs() < 1e-9,
            "baseline_cost_usd expected 0.30, got {}",
            result.baseline_cost_usd
        );
        // Saved = baseline − cost = 0.15 (no double-count)
        assert!(
            (result.saved_usd - 0.15).abs() < 1e-9,
            "saved_usd expected 0.15, got {}",
            result.saved_usd
        );
        // Body model called exactly 3 times
        let calls = stub.called_nodes();
        assert_eq!(
            calls.iter().filter(|id| *id == "m1").count(),
            3,
            "body model node must be called 3 times; calls: {calls:?}"
        );
    }

    // ---- W3a-2 Task 2: Loop termination / budget / cond tests -----------------

    /// Helper: minimal body workflow (t → m1 → o) with scripted model cost.
    /// The stub must have a response keyed "m1" for the body to succeed.
    fn make_loop_body_def(
        body_id: Uuid,
        cost_usd: f64,
        baseline_cost_usd: f64,
    ) -> (WorkflowDefinition, NodeOutput) {
        let def = WorkflowDefinition {
            triggers: vec![],
            id: body_id,
            version: 1,
            name: "body-wf".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "m1".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub".into(),
                        },
                        prompt: "{{input}}".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "m1".into(),
                    map: None,
                },
                Edge {
                    from: "m1".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        };
        let out = NodeOutput {
            content: json!("body_response"),
            cost_usd,
            baseline_cost_usd,
            model_used: Some("stub-model".into()),
            doc_vision_saved_est_usd: 0.0,
        };
        (def, out)
    }

    /// Helper: parent workflow with a single Loop node (t → lp → o).
    fn make_loop_parent(
        parent_id: Uuid,
        body_id: Uuid,
        cond: &str,
        max_iters: u32,
    ) -> WorkflowDefinition {
        WorkflowDefinition {
            triggers: vec![],
            id: parent_id,
            version: 1,
            name: "loop-parent".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "lp".into(),
                    kind: NodeKind::Loop {
                        body_workflow_id: body_id,
                        cond: cond.into(),
                        max_iters,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "lp".into(),
                    map: None,
                },
                Edge {
                    from: "lp".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        }
    }

    /// TERMINATION — max_iters hard cap: an always-truthy cond with max_iters=5
    /// must run the body EXACTLY 5 times then stop.  The test completing without
    /// hanging IS the termination proof.
    #[tokio::test]
    async fn loop_terminates_at_max_iters() {
        let body_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();
        let (body_def, body_out) = make_loop_body_def(body_id, 0.05, 0.10);
        // cond "{{input}}" resolves to "hello" (truthy) on every iteration.
        let parent_def = make_loop_parent(parent_id, body_id, "{{input}}", 5);

        let mut stub = StubExecutor::new(vec![("m1", body_out)]);
        stub.subworkflows.insert(body_id, body_def);

        let result = run_workflow(
            &stub,
            &parent_def,
            &json!("hello"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(
            result.status,
            WfStatus::Succeeded,
            "must Succeed after 5 iters"
        );

        let calls = stub.called_nodes();
        assert_eq!(
            calls.iter().filter(|id| *id == "m1").count(),
            5,
            "body must run exactly 5 times (max_iters=5); calls: {calls:?}"
        );
        // Cost rollup: 5 × 0.05
        assert!(
            (result.cost_usd - 0.25).abs() < 1e-9,
            "cost expected 0.25 (5×0.05), got {}",
            result.cost_usd
        );
    }

    /// COND EARLY EXIT — cond becomes false after iter 1.
    ///
    /// Cond: `{{lp}} == ""`
    ///   - BEFORE iter 1: loop-node output not yet set → `{{lp}}` resolves to ""
    ///     → "" == "" → TRUE → body runs.
    ///   - AFTER iter 1: loop node holds the serialized child.node_outputs array,
    ///     a non-empty JSON string → lhs != "" → FALSE → loop exits.
    /// Result: body runs exactly 1 time (< max_iters=5).
    #[tokio::test]
    async fn loop_cond_early_exit() {
        let body_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();
        let (body_def, body_out) = make_loop_body_def(body_id, 0.05, 0.10);
        let parent_def = make_loop_parent(parent_id, body_id, r#"{{lp}} == """#, 5);

        let mut stub = StubExecutor::new(vec![("m1", body_out)]);
        stub.subworkflows.insert(body_id, body_def);

        let result = run_workflow(
            &stub,
            &parent_def,
            &json!("hello"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        let calls = stub.called_nodes();
        let m1_count = calls.iter().filter(|id| *id == "m1").count();
        assert_eq!(
            m1_count, 1,
            "cond must exit after 1 iter (< max_iters=5); calls: {calls:?}"
        );
    }

    /// WHILE-SEMANTICS — cond false from the start: body runs 0 times.
    /// Cond "false" is an always-falsy literal (`is_truthy("false") == false`).
    /// Loop is a no-op: status Succeeded, cost==0.
    #[tokio::test]
    async fn loop_cond_false_first_iter_runs_zero() {
        let body_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();
        let (body_def, body_out) = make_loop_body_def(body_id, 0.05, 0.10);
        let parent_def = make_loop_parent(parent_id, body_id, "false", 5);

        let mut stub = StubExecutor::new(vec![("m1", body_out)]);
        stub.subworkflows.insert(body_id, body_def);

        let result = run_workflow(
            &stub,
            &parent_def,
            &json!("hello"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(
            result.status,
            WfStatus::Succeeded,
            "0-iter loop must Succeed"
        );
        let calls = stub.called_nodes();
        assert_eq!(
            calls.iter().filter(|id| *id == "m1").count(),
            0,
            "body must not run when cond is false from the start; calls: {calls:?}"
        );
        assert!(
            result.cost_usd.abs() < 1e-9,
            "0-iter loop cost must be 0; got {}",
            result.cost_usd
        );
    }

    /// BUDGET GATE — run_max_cost_usd=0.15 with body cost 0.10/iter stops the
    /// loop after exactly 2 iterations:
    ///   iter 1 gate: 0+0.00 < 0.15 → pass → cost=0.10
    ///   iter 2 gate: 0+0.10 < 0.15 → pass → cost=0.20
    ///   iter 3 gate: 0+0.20 ≥ 0.15 → STOP
    /// Proves the loop terminates before max_iters=10 when budget is exhausted.
    #[tokio::test]
    async fn loop_budget_stops_iterations() {
        let body_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();
        let (body_def, body_out) = make_loop_body_def(body_id, 0.10, 0.20);
        let parent_def = make_loop_parent(parent_id, body_id, "{{input}}", 10);

        let mut stub = StubExecutor::new(vec![("m1", body_out)]);
        stub.subworkflows.insert(body_id, body_def);

        let result = run_workflow(
            &stub,
            &parent_def,
            &json!("hello"),
            Some(0.15), // budget: allows iter 1+2, stops iter 3
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        // Loop exits cleanly (no child error) once the budget gate fires.
        assert_eq!(
            result.status,
            WfStatus::Succeeded,
            "budget-stopped loop must Succeed"
        );
        let calls = stub.called_nodes();
        let m1_count = calls.iter().filter(|id| *id == "m1").count();
        assert_eq!(
            m1_count, 2,
            "budget stops after 2 iters (< max_iters=10); calls: {calls:?}"
        );
        assert!(
            (result.cost_usd - 0.20).abs() < 1e-9,
            "cost must be 2×0.10=0.20; got {}",
            result.cost_usd
        );
    }

    /// NO-DOUBLE-COUNT — body cost=0.10 baseline=0.20, max_iters=3, always-truthy:
    ///   parent cost_usd       == 0.30  (3×0.10)
    ///   parent baseline_cost  == 0.60  (3×0.20)
    ///   parent saved_usd      == 0.30  (0.60−0.30; NOT 0.60 — double-count guard)
    #[tokio::test]
    async fn loop_cost_rolls_up_no_double_count() {
        let body_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();
        let (body_def, body_out) = make_loop_body_def(body_id, 0.10, 0.20);
        let parent_def = make_loop_parent(parent_id, body_id, "{{input}}", 3);

        let mut stub = StubExecutor::new(vec![("m1", body_out)]);
        stub.subworkflows.insert(body_id, body_def);

        let result = run_workflow(
            &stub,
            &parent_def,
            &json!("hello"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        assert!(
            (result.cost_usd - 0.30).abs() < 1e-9,
            "cost_usd must be 0.30 (3×0.10); got {}",
            result.cost_usd
        );
        assert!(
            (result.baseline_cost_usd - 0.60).abs() < 1e-9,
            "baseline_cost_usd must be 0.60 (3×0.20); got {}",
            result.baseline_cost_usd
        );
        // saved = baseline − cost = 0.30 (NOT 0.60 — double-count would be 0.60)
        assert!(
            (result.saved_usd - 0.30).abs() < 1e-9,
            "saved_usd must be 0.30 (not double-counted 0.60); got {}",
            result.saved_usd
        );
    }

    /// CHILD FAILURE PROPAGATION — a body that fails (stub returns ApiError for
    /// its model node) causes the loop to stop and the parent run to be Failed.
    #[tokio::test]
    async fn loop_child_failure_propagates() {
        let body_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();
        let (body_def, _) = make_loop_body_def(body_id, 0.05, 0.10);
        let parent_def = make_loop_parent(parent_id, body_id, "{{input}}", 10);

        // No response for "m1" → stub returns ApiError::Internal → child fails.
        let mut stub = StubExecutor::new(vec![]);
        stub.subworkflows.insert(body_id, body_def);

        let result = run_workflow(
            &stub,
            &parent_def,
            &json!("hello"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(
            result.status,
            WfStatus::Failed,
            "child failure must propagate to Failed; got {:?}",
            result.status
        );
        assert!(
            result.error.is_some(),
            "loop must report error when child fails"
        );
    }

    /// VALIDATION — max_iters bounds enforced by validate():
    ///   max_iters=0   → error (below minimum 1)
    ///   max_iters=101 → error (above maximum 100)
    ///   max_iters=50  → ok
    #[test]
    fn loop_max_iters_validation() {
        use crate::workflow::validate::validate;

        let body_id = Uuid::new_v4();

        let make_def = |max_iters: u32| WorkflowDefinition {
            triggers: vec![],
            id: Uuid::new_v4(),
            version: 1,
            name: "loop-val".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "lp".into(),
                    kind: NodeKind::Loop {
                        body_workflow_id: body_id,
                        cond: "{{input}}".into(),
                        max_iters,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "lp".into(),
                    map: None,
                },
                Edge {
                    from: "lp".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        };

        let any_model = |_: &str| true;

        // max_iters=0 → error
        let errs = validate(&make_def(0), &any_model).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("max_iters")),
            "max_iters=0 must produce a validation error; got: {errs:?}"
        );

        // max_iters=101 → error
        let errs = validate(&make_def(101), &any_model).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("max_iters")),
            "max_iters=101 must produce a validation error; got: {errs:?}"
        );

        // max_iters=50 → ok
        assert!(
            validate(&make_def(50), &any_model).is_ok(),
            "max_iters=50 must be valid"
        );
    }

    // ---- Execution-cap guard tests (W3a-2 follow-up) -------------------------

    /// NESTED LOOP DoS — two nested loops each with max_iters=100 over a
    /// zero-cost body (Trigger → Output, no model spend) would produce
    /// 100×100×2 ≈ 20 000 node executions — well above
    /// MAX_TOTAL_NODE_EXECUTIONS=10 000.  The cap must fire cleanly (status
    /// Failed, "execution" in error) long before completing all iterations.
    /// The test completing quickly IS the no-hang proof.
    ///
    /// NOTE: three levels of `run_workflow_boxed` async state machines stack up
    /// during `.await` polling. In unoptimised debug builds each frame can be
    /// several hundred KiB, so we spawn on a 32 MiB thread to avoid overflow.
    #[test]
    fn nested_loops_hit_execution_cap() {
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024) // 32 MiB for 3-level debug async nesting
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let leaf_id = Uuid::new_v4();
                        let inner_id = Uuid::new_v4();
                        let outer_id = Uuid::new_v4();

                        // leaf_wf: Trigger → Output (zero cost)
                        let leaf_def = make_leaf_def(leaf_id);

                        // inner_wf: Trigger → Loop(body=leaf_id, max_iters=100, cond="true") → Output
                        let inner_def = WorkflowDefinition {
                            triggers: vec![],
                            id: inner_id,
                            version: 1,
                            name: "inner-wf".into(),
                            nodes: vec![
                                Node {
                                    id: "t".into(),
                                    kind: NodeKind::Trigger,
                                },
                                Node {
                                    id: "lp".into(),
                                    kind: NodeKind::Loop {
                                        body_workflow_id: leaf_id,
                                        cond: "true".into(),
                                        max_iters: 100,
                                    },
                                },
                                Node {
                                    id: "o".into(),
                                    kind: NodeKind::Output,
                                },
                            ],
                            edges: vec![
                                Edge {
                                    from: "t".into(),
                                    to: "lp".into(),
                                    map: None,
                                },
                                Edge {
                                    from: "lp".into(),
                                    to: "o".into(),
                                    map: None,
                                },
                            ],
                            inputs: serde_json::Value::Null,
                            budget: BudgetPolicy::default(),
                            allowed_hosts: vec![],
                            metadata: serde_json::Value::Null,
                        };

                        // outer parent: Trigger → Loop(body=inner_id, max_iters=100, cond="true") → Output
                        let outer_def = WorkflowDefinition {
                            triggers: vec![],
                            id: outer_id,
                            version: 1,
                            name: "outer-wf".into(),
                            nodes: vec![
                                Node {
                                    id: "t".into(),
                                    kind: NodeKind::Trigger,
                                },
                                Node {
                                    id: "lp".into(),
                                    kind: NodeKind::Loop {
                                        body_workflow_id: inner_id,
                                        cond: "true".into(),
                                        max_iters: 100,
                                    },
                                },
                                Node {
                                    id: "o".into(),
                                    kind: NodeKind::Output,
                                },
                            ],
                            edges: vec![
                                Edge {
                                    from: "t".into(),
                                    to: "lp".into(),
                                    map: None,
                                },
                                Edge {
                                    from: "lp".into(),
                                    to: "o".into(),
                                    map: None,
                                },
                            ],
                            inputs: serde_json::Value::Null,
                            budget: BudgetPolicy::default(),
                            allowed_hosts: vec![],
                            metadata: serde_json::Value::Null,
                        };

                        let mut stub = StubExecutor::new(vec![]);
                        stub.subworkflows.insert(leaf_id, leaf_def);
                        stub.subworkflows.insert(inner_id, inner_def);

                        let result = run_workflow(
                            &stub,
                            &outer_def,
                            &json!("go"),
                            None,
                            |_| {},
                            None,
                            &HashMap::new(),
                            0,
                            &[],
                            &NoCache,
                        )
                        .await;

                        assert_eq!(
                            result.status,
                            WfStatus::Failed,
                            "nested loops must trip the execution cap and return Failed; got {:?}",
                            result.status
                        );
                        let err = result
                            .error
                            .expect("execution cap must produce an error string");
                        assert!(
                            err.contains("execution")
                                || err.contains("10000")
                                || err.contains("10_000"),
                            "error must mention executions/cap; got: {err}"
                        );
                    })
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// LEGIT WORKFLOW UNDER CAP — a small linear chain (t → m1 → m2 → o) plus
    /// a loop with max_iters=3 over a leaf body produces ≪ 10 000 node
    /// executions and must Succeed without tripping the cap.
    #[tokio::test]
    async fn normal_workflow_under_cap() {
        // Simple sequential workflow: t → m1 → m2 → o
        let def = make_sequential_def();
        let stub = StubExecutor::new(vec![
            (
                "m1",
                NodeOutput {
                    content: json!("r1"),
                    cost_usd: 0.10,
                    baseline_cost_usd: 0.20,
                    model_used: Some("haiku".into()),
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("r2"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.10,
                    model_used: Some("haiku".into()),
                    doc_vision_saved_est_usd: 0.0,
                },
            ),
        ]);

        let result = run_workflow(
            &stub,
            &def,
            &json!("hello"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(
            result.status,
            WfStatus::Succeeded,
            "normal workflow must Succeed without hitting the execution cap"
        );
    }

    /// LOOP EXECUTIONS COUNTED — a single loop with max_iters=5 over a
    /// zero-cost leaf body (Trigger → Output, 2 nodes each) runs 5 body
    /// iterations = 5×2 + 3 parent nodes = 13 total executions, well under
    /// MAX_TOTAL_NODE_EXECUTIONS=10 000.  The run must Succeed, proving the
    /// counter does not falsely trip the cap for well-behaved loops.
    #[tokio::test]
    async fn loop_executions_counted() {
        let body_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();

        // Leaf body: Trigger → Output (zero cost, 2 nodes per iteration)
        let body_def = make_leaf_def(body_id);

        // Parent: t → lp (Loop, max_iters=5, cond="true") → o
        let parent_def = make_loop_parent(parent_id, body_id, "true", 5);

        let mut stub = StubExecutor::new(vec![]);
        stub.subworkflows.insert(body_id, body_def);

        let result = run_workflow(
            &stub,
            &parent_def,
            &json!("go"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        // 5 iterations × 2 nodes/iter + 3 parent nodes = 13 total < 10 000 cap
        assert_eq!(
            result.status,
            WfStatus::Succeeded,
            "5-iter loop (13 total executions) must not trip the cap; got {:?} (err: {:?})",
            result.status,
            result.error
        );
    }

    // ---- D6 Slice 3: Document-node cache ----------------------------------

    /// A recording `DistillCacheStore`: a pre-seeded hit map + an append-only log
    /// of get/upsert calls. Used to prove the Document node consults the cache
    /// (hit → cached text, no sidecar; miss → upsert after the distill).
    struct RecordingCache {
        /// Pre-seeded cache hits keyed by `content_hash`.
        hits: std::sync::Mutex<HashMap<String, CachedDistill>>,
        /// Append-only log of `(op, content_hash)` calls.
        ops: std::sync::Mutex<Vec<(&'static str, String)>>,
        /// The text an upsert stores (so a get-after-upsert returns it).
        upserted: std::sync::Mutex<HashMap<String, CachedDistill>>,
    }

    impl RecordingCache {
        fn empty() -> Self {
            RecordingCache {
                hits: std::sync::Mutex::new(HashMap::new()),
                ops: std::sync::Mutex::new(Vec::new()),
                upserted: std::sync::Mutex::new(HashMap::new()),
            }
        }
        fn seed(content_hash: &str, text: &str) -> Self {
            let cache = Self::empty();
            cache.hits.lock().unwrap().insert(
                content_hash.to_string(),
                CachedDistill {
                    text: text.to_string(),
                    pages: 1,
                    engine: "pdf-extract".to_string(),
                },
            );
            cache
        }
        fn ops(&self) -> Vec<(String, String)> {
            self.ops
                .lock()
                .unwrap()
                .iter()
                .map(|(op, h)| (op.to_string(), h.clone()))
                .collect()
        }
    }

    #[async_trait]
    impl DistillCacheStore for RecordingCache {
        async fn get(&self, key: &DistillCacheKey) -> Option<CachedDistill> {
            self.ops
                .lock()
                .unwrap()
                .push(("get", key.content_hash.clone()));
            // A hit from the seed OR a prior upsert.
            self.hits
                .lock()
                .unwrap()
                .get(&key.content_hash)
                .cloned()
                .or_else(|| {
                    self.upserted
                        .lock()
                        .unwrap()
                        .get(&key.content_hash)
                        .cloned()
                })
        }
        async fn upsert(&self, key: &DistillCacheKey, doc: &CachedDistill) {
            self.ops
                .lock()
                .unwrap()
                .push(("upsert", key.content_hash.clone()));
            self.upserted
                .lock()
                .unwrap()
                .insert(key.content_hash.clone(), doc.clone());
        }
    }

    /// Build a minimal Trigger → Document → Output flow whose Document node
    /// sources a base64 PDF whose content hash is `hash_of(data)`.
    fn doc_node_flow(data: &str) -> WorkflowDefinition {
        let expected_hash = blake3::hash(data.as_bytes()).to_hex().to_string();
        let _ = expected_hash; // the node computes this at runtime; here for reference.
        WorkflowDefinition {
            triggers: vec![],
            id: Uuid::nil(),
            version: 1,
            name: "doc".to_string(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "d".into(),
                    kind: NodeKind::Document {
                        source: tt_shared::messages::DocumentSource::Base64 {
                            media_type: "application/pdf".into(),
                            data: data.to_string(),
                        },
                        cache_key: None,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "d".into(),
                    map: None,
                },
                Edge {
                    from: "d".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: Vec::new(),
            metadata: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn document_node_cache_hit_returns_cached_text_no_sidecar() {
        // A pre-seeded cache hit → the Document node emits the cached text +
        // does NOT consult the sidecar (no sidecar is configured, so a cache
        // MISS would fail; the hit short-circuits before the distill).
        std::env::remove_var("TT_DOC_SIDECAR_URL");
        let data = "JVBERi0="; // the cached text is keyed on blake3(this).
        let cache_hash = blake3::hash(data.as_bytes()).to_hex().to_string();
        let cache = RecordingCache::seed(&cache_hash, "cached-distilled-text");
        let def = doc_node_flow(data);
        let result = run_workflow(
            &StubExecutor::new(vec![]),
            &def,
            &json!("go"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &cache,
        )
        .await;
        assert_eq!(result.status, WfStatus::Succeeded);
        // The cache was consulted (a get), not upserted (no sidecar distill ran).
        let ops = cache.ops();
        assert!(ops.iter().any(|(o, _)| o == "get"), "cache get must fire");
        assert!(
            !ops.iter().any(|(o, _)| o == "upsert"),
            "a cache hit must not upsert (no distill ran)"
        );
        // The Output node aggregates its upstream (the Document node) — so its
        // content carries the cached distilled text (the engine's `node_outputs`
        // is the Output node's collected set, keyed by the Output node id "o").
        let out_node = result
            .node_outputs
            .iter()
            .find(|(id, _)| id == "o")
            .expect("output node present")
            .1
            .clone();
        assert_eq!(
            out_node.content,
            json!("cached-distilled-text"),
            "a cache hit → the Output node carries the cached text"
        );
    }

    #[tokio::test]
    async fn document_node_no_cache_no_sidecar_fails_loud() {
        // NoCache + no sidecar → the Document node fails loud (error output),
        // the run still Succeeds (a failed node surfaces in its output, not a
        // run abort — the Output node collects downstream). Mirrors the
        // fail-loud posture the owner approved.
        std::env::remove_var("TT_DOC_SIDECAR_URL");
        let def = doc_node_flow("JVBERi0=");
        let result = run_workflow(
            &StubExecutor::new(vec![]),
            &def,
            &json!("go"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;
        // The Output node carries the Document's fail-loud null output (no cache
        // hit + no sidecar → the node emits null + journals a failure; the run
        // itself still Succeeds — a per-node failure surfaces in the node output,
        // the Output node collects it).
        assert_eq!(result.status, WfStatus::Succeeded);
        let out_node = result
            .node_outputs
            .iter()
            .find(|(id, _)| id == "o")
            .expect("output node present")
            .1
            .clone();
        assert!(
            out_node.content.is_null(),
            "no cache + no sidecar → the Document node's fail-loud null propagates to the Output; got: {:?}",
            out_node.content
        );
    }
}
