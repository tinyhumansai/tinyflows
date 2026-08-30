{
    // Cooperative cancellation, checked at the node boundary before
    // any real work. When the run's token is cancelled this node
    // becomes a no-op: it emits an empty update on the default port
    // and — crucially — does **not** fan out (a plain `Update`, not
    // `emit`), so a fan-out node's parallel successors are not
    // scheduled. Downstream nodes reached by static edges will hit
    // this same check and no-op in turn, so the run winds down without
    // starting further node work. The engine reports it as cancelled.
    if token.is_cancelled() {
        tracing::info!(node = %node.id, "run cancelled; skipping node work");
        let mut update = items_update(&node.id, &[], None)?;
        if lane.is_none() {
            stamp_activation_step(&mut update, &node.id, ctx.step);
        }
        return Ok(NodeResult::Update(update));
    }
    
    if is_trigger {
        // The trigger payload is pre-seeded into the state; no-op update
        // (still fanning out if the trigger has parallel successors).
        return Ok(emit(json!({}), None, &[]));
    }
    
    // Human-in-the-loop approval gate. A node whose config sets
    // `requires_approval: true` must not execute until its id is
    // listed in the run input's `approvals` array (readable at
    // `state["run"]["trigger"]["approvals"]`). Until then it pauses
    // the run via a graph interrupt, so its downstream never
    // runs and the run reports the pending node.
    let requires_approval = node
        .config
        .get("requires_approval")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if requires_approval {
        // Human-in-the-loop **denial**. A resume delivered with a
        // structured value `{ "rejected": [<gate id>, …] }` (see
        // `resume_with_checkpointer_journaled_observed`) denies the
        // named gate rather than approving it: the gate emits an
        // error item on its `error` port when one is wired (so a
        // recovery branch can handle the rejection), or fails the run
        // when it has no `error` port. Checked before the approval
        // branch so a denial always wins over the bare-resume approval.
        let denied = resume_value
            .as_ref()
            .and_then(|v| v.get("rejected"))
            .and_then(Value::as_array)
            .is_some_and(|rejected| {
                rejected
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|id| id == node.id)
            });
        if denied {
            tracing::info!(node = %node.id, has_error_edge, "approval gate denied");
            let item = Item::new(json!({
                "error": {
                    "message": "approval denied",
                    "node": node.id,
                    "denied": true,
                }
            }));
            if has_error_edge {
                // Route the denial to the `error` port so a recovery
                // sub-graph runs. Use `emit`: when the gate's error-port
                // recovery edges fan out (≥2 same-port successors) the
                // node is command-routed and has no conditional router to
                // key on the recorded port, so the branches must be driven
                // directly via a `Command::goto`; a single/mixed-port error
                // edge falls back to a plain update the conditional-edge
                // router consumes.
                return Ok(emit(
                    items_update(&node.id, std::slice::from_ref(&item), Some("error"))?,
                    Some("error"),
                    std::slice::from_ref(&item),
                ));
            }
            // No error branch to route to — fail the run so the denial
            // is not silently swallowed.
            return Err(GraphError::Graph(format!(
                "approval gate '{}' was denied and has no `error` port to route to",
                node.id
            )));
        }
        let approved = state.get("run").is_some_and(|run| {
            // Two places, because approvals reach a run two
            // ways: inside an object trigger payload (the
            // original spelling, kept working) and through
            // `RunInput::with_approvals`, which is the only one
            // available when the trigger is not an object.
            let listed = |approvals: Option<&Value>| {
                approvals.and_then(Value::as_array).is_some_and(|ids| {
                    ids.iter().filter_map(Value::as_str).any(|id| id == node.id)
                })
            };
            listed(run.get("approvals"))
                || listed(
                    run.get("trigger")
                        .and_then(|trigger| trigger.get("approvals")),
                )
        });
        // `approved_by_resume` is set when a checkpointed resume
        // delivered an approval (bare `true`, or this gate listed in
        // the structured `approved` array) to this interrupted gate.
        if !approved && !approved_by_resume {
            tracing::info!(node = %node.id, "node paused awaiting approval");
            let payload = if node.config.is_null() {
                json!({})
            } else {
                node.config.clone()
            };
            return Ok(NodeResult::Interrupt(Interrupt {
                id: node.id.clone(),
                node: node.id.clone().into(),
                payload,
            }));
        }
    }
}
