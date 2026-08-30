use super::super::*;

const BACKOFF_POLL_MS: u64 = 25;

#[allow(clippy::too_many_arguments)]
pub(super) async fn finish_execution<F>(
    output: Option<crate::nodes::NodeOutput>,
    last_err: Option<EngineError>,
    // How many times the executor actually ran. Reported on the
    // retries-exhausted warning so "after retries" carries a number.
    attempts: u32,
    duration_ms: u128,
    on_error: &str,
    node: &crate::model::Node,
    input: &[Item],
    run_meta: &Value,
    nodes_state: &Value,
    caps: &Capabilities,
    agents: &Arc<Vec<crate::model::AgentDefinition>>,
    observer: &Arc<dyn RunObserver>,
    token: &CancellationToken,
    lane: &Option<crate::nodes::LaneContext>,
    resume_value: &Option<Value>,
    activation_step: usize,
    steps: &Arc<Mutex<Vec<ExecutionStep>>>,
    terminal_error: &Arc<Mutex<Option<EngineError>>>,
    plain_targets: &[String],
    emit: F,
) -> crate::graph::Result<NodeResult<Value>>
where
    F: Fn(Value, Option<&str>, &[Item]) -> NodeResult<Value> + Send + Sync,
{
    match output {
        Some(output) => {
            tracing::debug!(node = %node.id, ?node.kind, item_count = output.items.len(), "node executed");
            let step = ExecutionStep {
                node_id: node.id.clone(),
                status: StepStatus::Success,
                output: serde_json::to_value(&output.items).unwrap_or(Value::Null),
                duration_ms,
                diagnostics: output.diagnostics.clone(),
                // Carried, not interpreted: an `agent` node's executor put the
                // host's folded transcript here, every other node left it empty.
                transcript: output.transcript.clone(),
            };
            steps
                .lock()
                .expect("steps mutex poisoned")
                .push(step.clone());
            observer.on_step_finish(&step);
            let port = output.port.as_deref();

            // A node that asked for something other than plain data
            // flow. Handled before `emit` because both variants
            // bypass ordinary routing.
            match output.control {
                // Pause the run. The update is deliberately dropped:
                // the underlying executor discards an interrupting
                // activation's state write, so returning one here
                // would be a silent lie about what got committed.
                // The node re-runs from the top on resume.
                Some(crate::nodes::NodeControl::Interrupt { id, payload }) => {
                    tracing::info!(node = %node.id, gate = %id, "node paused the run");
                    return Ok(NodeResult::Interrupt(Interrupt {
                        id,
                        node: node.id.clone().into(),
                        payload,
                    }));
                }
                // Ask to be run again. The update *is* committed, so
                // the node can leave itself notes (a poll count, a
                // ticket) and read them back on the next activation.
                //
                // `goto`-ing ourselves re-activates this node in the
                // next super-step. Each poll costs one super-step and
                // one node visit, so the caller's own bounded poll
                // count is what stops this — not the run-level
                // backstop, which cannot say which node span.
                Some(crate::nodes::NodeControl::Reenter { after_ms }) => {
                    let update = items_update_with_meta(
                        &node.id,
                        &output.items,
                        port,
                        output.meta.as_ref(),
                    )?;
                    if after_ms > 0 {
                        // Chopped into slices so a cancel during a
                        // long poll interval is seen promptly, the
                        // same way retry backoff is drained above.
                        let mut remaining = after_ms;
                        while remaining > 0 {
                            if token.is_cancelled() {
                                tracing::info!(node = %node.id, "run cancelled while waiting to re-enter");
                                return Ok(NodeResult::Update(items_update(&node.id, &[], None)?));
                            }
                            let slice = remaining.min(BACKOFF_POLL_MS);
                            // Always yields, so the tasks this node is polling
                            // for get a turn before it looks again — see
                            // `super::backoff`.
                            super::backoff::wait_slice(slice).await;
                            remaining -= slice;
                        }
                    }
                    return Ok(NodeResult::Command(
                        Command::goto(vec![node.id.clone()]).with_update(update),
                    ));
                }
                // Open a lane per entry: schedule every successor
                // once for each, each carrying its own work.
                //
                // This is the one routing decision a node makes that
                // the graph's edges cannot express. `Send` packets
                // are the reason it works at all — plain
                // activations dedupe by node id, so repeating a
                // target would collapse back to one.
                Some(crate::nodes::NodeControl::Scatter { lanes }) => {
                    let count = lanes.len();
                    let mut routed: Vec<RouteTarget> = Vec::new();
                    for (index, lane_items) in lanes.iter().enumerate() {
                        for target in plain_targets {
                            let envelope = lane_envelope(&node.id, index, count, lane_items)
                                .unwrap_or(Value::Null);
                            routed.push(RouteTarget::Send(crate::graph::Send::new(
                                target.clone(),
                                envelope,
                            )));
                        }
                    }
                    // The scatter's own slot records how many lanes
                    // it opened; a gather counts arrivals against it
                    // rather than guessing from what turned up.
                    let mut update = items_update_with_meta(
                        &node.id,
                        &output.items,
                        port,
                        output.meta.as_ref(),
                    )?;
                    stamp_activation_step(&mut update, &node.id, activation_step);
                    tracing::debug!(
                        node = %node.id, lanes = count, "scatter: opened lanes"
                    );
                    return Ok(NodeResult::Command(
                        Command::route(routed).with_update(update),
                    ));
                }
                None => {}
            }

            // A lane activation writes its own lane slot; only a
            // non-lane activation owns the node's top-level slot.
            let update = match lane.as_ref() {
                Some(lane) => lane_items_update(
                    &node.id,
                    lane,
                    &output.items,
                    port,
                    "ok",
                    output.meta.as_ref(),
                )?,
                None => {
                    items_update_with_meta(&node.id, &output.items, port, output.meta.as_ref())?
                }
            };
            Ok(emit(update, port, &output.items))
        }
        None => {
            // Report the cause, not just the fact. `last_err` is a parameter of
            // this function and is not unwrapped until the `let Some(err)`
            // below, so without naming it here nothing in the log could say
            // why the node failed — the line was node id and nothing else.
            // Borrowed, not moved: the unwrap below still takes it by value.
            //
            // Deliberately logged here rather than after the unwrap: the `None`
            // arm below is the defensive unreachable path, and moving this line
            // past it would stop that case reporting at all.
            tracing::warn!(
                node = %node.id,
                attempts,
                on_error = %on_error,
                error = ?last_err,
                "node failed after retries"
            );
            // Recover the data-binding diagnostics of the failed
            // attempt: the executor computes them while resolving config
            // but discards them on the error path, so re-resolve the same
            // config against the same scope to capture which
            // `=`-expressions resolved to null (e.g. a null arg that made
            // the tool error). Deterministic given identical input, so
            // this reproduces the last failed attempt's diagnostics; done
            // without re-warning to avoid duplicate log lines.
            let diagnostics = {
                let ctx = NodeContext {
                    node,
                    input,
                    run: run_meta,
                    nodes: nodes_state,
                    caps,
                    agents,
                    observer: observer.as_ref(),
                    token: token.clone(),
                    lane: lane.clone(),
                    resume: resume_value.clone(),
                    step: activation_step,
                };
                let scope = crate::nodes::expr_scope(&ctx);
                crate::expr::resolve_traced(&node.config, &scope).1
            };
            let step = ExecutionStep {
                node_id: node.id.clone(),
                status: StepStatus::Error,
                output: Value::Null,
                duration_ms,
                diagnostics,
                // A node that failed produced no `NodeOutput`, so there is no
                // outcome to read a transcript from.
                transcript: Vec::new(),
            };
            steps
                .lock()
                .expect("steps mutex poisoned")
                .push(step.clone());
            observer.on_step_finish(&step);
            // Retries exhausted. `last_err` is always set when the loop
            // ran (`max_attempts >= 1`); the `None` arm is unreachable
            // but handled defensively — emit an empty update, never panic.
            let Some(err) = last_err else {
                return Ok(emit(items_update(&node.id, &[], None)?, None, &[]));
            };
            match on_error {
                // Turn the failure into data on the default port.
                "continue" => {
                    let item = error_item(&node.id, &err);
                    let update = match lane.as_ref() {
                        Some(lane) => lane_items_update(
                            &node.id,
                            lane,
                            std::slice::from_ref(&item),
                            None,
                            "ok",
                            None,
                        )?,
                        None => items_update(&node.id, std::slice::from_ref(&item), None)?,
                    };
                    Ok(emit(update, None, std::slice::from_ref(&item)))
                }
                // Turn the failure into data on the `error` port so the
                // graph can route it to a recovery sub-graph.
                "route" => {
                    let item = error_item(&node.id, &err);
                    let update = match lane.as_ref() {
                        Some(lane) => lane_items_update(
                            &node.id,
                            lane,
                            std::slice::from_ref(&item),
                            Some("error"),
                            "ok",
                            None,
                        )?,
                        None => items_update(&node.id, std::slice::from_ref(&item), Some("error"))?,
                    };
                    Ok(emit(update, Some("error"), std::slice::from_ref(&item)))
                }
                // "stop" (default) and any unknown policy fail the run.
                //
                // Stash the structured error before handing
                // the runtime a string: `GraphError::Graph` is
                // the only channel out of a handler, so without this
                // every node failure would reach the caller flattened
                // into `EngineError::Capability` and a host could not
                // tell a loop that ran away from an HTTP call that
                // 500'd. `build_and_run` prefers whatever lands here.
                // First writer wins: the failure that ends the run is
                // the one worth reporting, and a parallel superstep
                // may produce several.
                _ => {
                    let message = err.to_string();
                    // A lane failure belongs to its gather, whose
                    // `on_lane_error` policy decides whether to collect, skip,
                    // or fail the overall run.
                    if let Some(lane) = lane.as_ref() {
                        let meta = json!({ "error": message });
                        let update =
                            lane_items_update(&node.id, lane, &[], None, "failed", Some(&meta))?;
                        return Ok(emit(update, None, &[]));
                    }
                    let mut slot = terminal_error
                        .lock()
                        .expect("terminal error mutex poisoned");
                    if slot.is_none() {
                        *slot = Some(err);
                    }
                    drop(slot);
                    // The message still travels the string channel
                    // unchanged, so every path that only sees the
                    // stringified error reads exactly what it did
                    // before; the sink is strictly additive.
                    Err(GraphError::Graph(message))
                }
            }
        }
    }
}
