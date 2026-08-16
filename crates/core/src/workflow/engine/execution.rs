//! Workflow DAG execution loop.
//!
//! This module owns mutable run state, node scheduling, recursive loop and
//! sub-workflow execution, journaling callbacks, and event emission. The parent
//! module remains the public engine facade and result contract.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn run_workflow_boxed<'a>(
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
                let mut captured_inputs: HashMap<String, Option<serde_json::Value>> =
                    HashMap::new();
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
                            captured_inputs.insert(
                                node_id.clone(),
                                capture_input(prompt, &trigger_id, &outputs, variables),
                            );
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
                            captured_inputs.insert(
                                node_id.clone(),
                                capture_input(prompt, &trigger_id, &outputs, variables),
                            );
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
                                input: captured_inputs.get(&node_id).cloned().flatten(),
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
                                input: captured_inputs.get(&node_id).cloned().flatten(),
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
                            input: capture_input(expr, &trigger_id, &outputs, variables),
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
                            input: capture_input(cond, &trigger_id, &outputs, variables),
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
                                // Include status so downstream nodes can branch on it. For the
                                // same reason no `input` capture is recorded here — the wire
                                // spec is built with substitute_with_secrets and must never be
                                // persisted, value-free or not.
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
                                    input: None,
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
                                    input: None,
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
                            // Loop `cond` is re-evaluated each iteration against
                            // evolving outputs; the capture reflects the final
                            // iteration's evaluation context.
                            input: capture_input(cond, &trigger_id, &outputs, variables),
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
                            // Child inputs equal the parent run's inputs (MVP);
                            // capturing a full copy is redundant + unbounded, and
                            // the run record already scopes them.
                            input: None,
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
                    // D6 — distill a document's text layer for downstream nodes.
                    // A complete semantic cache hit skips extraction. A miss uses
                    // the Document Lane seam; sidecar failure emits an error
                    // NodeOutput rather than mislabeling raw bytes as text. The
                    // isolated `doc_vision_saved_est_usd` remains $0 here because
                    // the downstream served model—and therefore avoided vision
                    // cost—is not known at this node.
                    NodeKind::Document { source, cache_key } => {
                        // Resolve the document bytes before constructing the
                        // complete semantic cache identity.
                        // Resolve the document's bytes: an inline base64 source
                        // passes through untouched; a URL source is fetched
                        // through the gateway's guarded egress (the SAME
                        // no-redirect/DNS-guard/https-only transport the Http
                        // node uses, re-asserting the shared SSRF guard, the
                        // closed media set, and the shared byte cap at run
                        // time). Fail closed: any fetch error / unsupported
                        // media / oversize body → a named-error failed
                        // NodeOutput, never raw remote bytes downstream.
                        let (media_type, data_b64) = match source {
                            tt_shared::messages::DocumentSource::Base64 { media_type, data } => {
                                (media_type.clone(), data.clone())
                            }
                            tt_shared::messages::DocumentSource::Url { url } => {
                                match wf_http::run_document_fetch(url).await {
                                    Ok(doc) => (doc.media_type, doc.data_b64),
                                    Err(e) => {
                                        // SECURITY: HttpError strings are
                                        // sanitized — the URL is never echoed.
                                        let err = format!("document node source fetch failed: {e}");
                                        journal(NodeJournalEntry {
                                            node_id: node_id.clone(),
                                            status: "failed".into(),
                                            output: None,
                                            input: None,
                                            cost_usd: 0.0,
                                            model_used: None,
                                            error: Some(err),
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
                                            budget_remaining_usd: run_max_cost_usd
                                                .map(|m| m - accrued),
                                        });
                                        done.insert(node_id.clone());
                                        continue;
                                    }
                                }
                            }
                        };
                        // Cache reuse requires decoded-byte identity, normalized
                        // media type, caller key, an immutable sidecar revision,
                        // and the TokenTrimmer policy revision. Missing/invalid
                        // provenance disables reuse but never extraction.
                        let caller_key = cache_key
                            .as_ref()
                            .map(|value| substitute(value, &trigger_id, &outputs, variables));
                        let harness = crate::document_lane::seam::DistillHarness::from_env();
                        let key = harness.cache_revision.as_ref().and_then(|revision| {
                            let bytes = base64::engine::general_purpose::STANDARD
                                .decode(data_b64.trim())
                                .ok()?;
                            Some(DistillCacheKey::new(
                                blake3::hash(&bytes).to_hex().to_string(),
                                caller_key,
                                &media_type,
                                revision,
                            ))
                        });
                        let cached = match key.as_ref() {
                            Some(key) => cache.get(key).await,
                            None => None,
                        };
                        if let Some(cached) = cached {
                            let content = serde_json::Value::String(cached.text.clone());
                            journal(NodeJournalEntry {
                                node_id: node_id.clone(),
                                status: "completed".into(),
                                output: Some(content.clone()),
                                input: None,
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
                        let distilled_outcome = crate::document_lane::seam::distill_part(
                            &harness,
                            &media_type,
                            &data_b64,
                        )
                        .await;
                        let out = match distilled_outcome {
                            crate::document_lane::seam::DistillOutcome::Distilled {
                                text,
                                engine,
                                pages,
                                ..
                            } => {
                                if let Some(key) = key.as_ref() {
                                    cache
                                        .upsert(
                                            key,
                                            &CachedDistill {
                                                text: text.clone(),
                                                pages,
                                                engine,
                                            },
                                        )
                                        .await;
                                }
                                let content = serde_json::Value::String(text.clone());
                                journal(NodeJournalEntry {
                                    node_id: node_id.clone(),
                                    status: "completed".into(),
                                    output: Some(content.clone()),
                                    input: None,
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
                                    input: None,
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
