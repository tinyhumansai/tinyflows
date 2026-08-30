/// A run trace, bounded and optionally narrowed to one node.
fn project_trace(trace: &RunTrace, node_id: Option<&str>) -> Value {
    let steps: Vec<_> = match node_id {
        Some(id) => trace.steps_for(id).into_iter().cloned().collect(),
        None => trace.steps.clone(),
    };
    let value = json!({
        "summary": trace.summary(),
        "steps": steps,
        "calls": trace.calls,
        "diagnosis": trace.diagnosis,
    });
    // Bounded because a run over a large payload would otherwise put the whole
    // thing in a context window.
    crate::evidence::bounded_evidence(&value)
}

/// The null bindings, projected as the pointer they are meant to be.
fn null_binding_report(trace: &RunTrace) -> Vec<Value> {
    trace
        .null_bindings()
        .into_iter()
        .map(|(node, binding)| {
            json!({
                "nodeId": node,
                "location": binding.location,
                "expression": binding.expression,
                "readsFrom": binding.reads_from,
            })
        })
        .collect()
}

fn str_arg(args: &Value, name: &str) -> Result<String, ToolError> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::InvalidArguments,
                format!("missing required string argument {name:?}"),
            )
        })
}

fn graph_arg(args: &Value) -> Result<WorkflowGraph, ToolError> {
    let graph = args.get("graph").ok_or_else(|| {
        ToolError::new(
            ToolErrorCode::InvalidArguments,
            "missing required argument \"graph\"".to_string(),
        )
    })?;
    serde_json::from_value(graph.clone()).map_err(|err| {
        ToolError::new(
            ToolErrorCode::InvalidGraph,
            format!("the graph did not parse: {err}"),
        )
    })
}

fn run_input(args: &Value) -> RunInput {
    let mut input = RunInput::new(args.get("trigger").cloned().unwrap_or(Value::Null));
    if let Some(Value::Object(inputs)) = args.get("inputs") {
        input.inputs = inputs.clone();
    }
    if let Some(Value::Array(approvals)) = args.get("approvals") {
        input.approvals = approvals
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
    }
    input
}

/// Build the mock rules a call programmed.
fn mocks_from(args: &Value) -> Result<MockCaps, ToolError> {
    let mut mocks = MockCaps::new();
    let Some(Value::Array(rules)) = args.get("mocks") else {
        return Ok(mocks);
    };
    for rule in rules {
        let capability = rule
            .get("capability")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolError::new(
                    ToolErrorCode::InvalidArguments,
                    "each mock rule needs a capability".to_string(),
                )
            })?;
        let target = rule.get("target").and_then(Value::as_str).unwrap_or("*");
        let respond = respond_from(rule)?;
        mocks = match capability {
            "tools" => mocks.on_tool(target, respond),
            "http" => mocks.on_http(target, respond),
            "llm" => mocks.on_llm(respond),
            "agent" => mocks.on_agent(target, respond),
            "code" => mocks.on_code(respond),
            "shell" => mocks.on_shell(respond),
            other => {
                return Err(ToolError::new(
                    ToolErrorCode::InvalidArguments,
                    format!("unknown capability {other:?}"),
                ));
            }
        };
        if let Some(node_id) = rule.get("node_id").and_then(Value::as_str) {
            mocks = mocks.only_from(node_id);
        }
    }
    Ok(mocks)
}

/// One programmed response.
fn respond_from(rule: &Value) -> Result<Respond, ToolError> {
    let base = if let Some(Value::Array(entries)) = rule.get("sequence") {
        let mut sequence = Vec::new();
        for entry in entries {
            sequence.push(respond_from(entry)?);
        }
        Respond::Sequence(sequence)
    } else if let Some(error) = rule.get("error").and_then(Value::as_str) {
        Respond::error(error)
    } else if let Some(schema) = rule.get("schema") {
        Respond::schema(schema.clone())
    } else if let Some(value) = rule.get("value") {
        Respond::value(value.clone())
    } else {
        Respond::Echo
    };
    Ok(match rule.get("delay_ms").and_then(Value::as_u64) {
        Some(ms) => Respond::after(Duration::from_millis(ms), base),
        None => base,
    })
}

/// The breakpoint a `flow_debug.breakpoint` call described.
fn breakpoint_spec(args: &Value) -> Result<BreakpointSpec, ToolError> {
    let any = args.get("any").and_then(Value::as_bool).unwrap_or(false);
    let target = match (any, args.get("node_id").and_then(Value::as_str)) {
        (true, _) => NodeTarget::Any,
        (false, Some(id)) => NodeTarget::Id(id.to_string()),
        (false, None) => {
            return Err(ToolError::new(
                ToolErrorCode::InvalidArguments,
                "a breakpoint needs a node_id, or any:true for every node".to_string(),
            ));
        }
    };

    let on_error = args
        .get("on_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // An on-error breakpoint has to break *after* the node — that is the only
    // phase at which a failure exists — so default the phase to match the
    // intent rather than making the caller work it out.
    let before = args
        .get("before")
        .and_then(Value::as_bool)
        .unwrap_or(!on_error);
    let after = args
        .get("after")
        .and_then(Value::as_bool)
        .unwrap_or(on_error);

    let mut conditions = Vec::new();
    if on_error {
        conditions.push(Condition::OnError);
    }
    if let Some(n) = args.get("activation").and_then(Value::as_u64) {
        conditions.push(Condition::Activation(n as u32));
    }
    if let Some(expr) = args.get("expr").and_then(Value::as_str) {
        conditions.push(Condition::Expr(expr.to_string()));
    }
    let condition = match conditions.len() {
        0 => Condition::Always,
        1 => conditions.remove(0),
        _ => Condition::All(conditions),
    };

    Ok(BreakpointSpec {
        target,
        before,
        after,
        condition,
        mode: PauseMode::Live,
        max_hits: args
            .get("once")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then_some(1),
    })
}

/// The command a `flow_debug.release` call described.
fn debug_command(args: &Value) -> Result<DebugCommand, ToolError> {
    let command = args.get("command").and_then(Value::as_str).ok_or_else(|| {
        ToolError::new(
            ToolErrorCode::InvalidArguments,
            "release needs a command".to_string(),
        )
    })?;
    Ok(match command {
        "continue" => DebugCommand::Continue,
        "step" => DebugCommand::Step,
        "skip" => DebugCommand::Skip,
        "detach" => DebugCommand::Detach,
        "fail" => DebugCommand::Fail(
            args.get("message")
                .and_then(Value::as_str)
                .unwrap_or("failed from the debugger")
                .to_string(),
        ),
        "patch" => DebugCommand::Patch(
            args.get("patch")
                .cloned()
                .unwrap_or(Value::Object(Map::new())),
        ),
        "override" => {
            let items = match args.get("items") {
                Some(Value::Array(values)) => values.iter().map(json_to_item).collect(),
                Some(single) => vec![json_to_item(single)],
                None => Vec::new(),
            };
            DebugCommand::Override {
                items,
                port: args.get("port").and_then(Value::as_str).map(str::to_string),
            }
        }
        other => {
            return Err(ToolError::new(
                ToolErrorCode::InvalidArguments,
                format!("unknown command {other:?}"),
            ));
        }
    })
}

/// Accept either a full item (`{"json": …}`) or a bare payload.
///
/// An agent writing `items: [{"ok": true}]` means the payload; requiring the
/// envelope would be a papercut with no upside.
fn json_to_item(value: &Value) -> Item {
    match value.get("json") {
        Some(payload) => Item::new(payload.clone()),
        None => Item::new(value.clone()),
    }
}

