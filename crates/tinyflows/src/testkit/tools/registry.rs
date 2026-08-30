use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::compiler::compile;
use crate::data::Item;
use crate::engine::RunInput;
use crate::model::WorkflowGraph;
use crate::testkit::debug::{
    BreakpointSpec, Condition, DebugCommand, DebugSession, NodeTarget, PauseMode,
};
use crate::testkit::mocks::{MockCaps, Respond};
use crate::testkit::trace::RunTrace;

use super::error::{ToolError, ToolErrorCode};

/// How long an untouched session is kept before being reaped.
///
/// A session holds a spawned run, so one an agent forgot about must not be held
/// for the life of the process.
pub const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(900);

/// A live debug session plus the bookkeeping the registry needs.
///
/// The controller is held separately from the session, and deliberately so:
/// setting a breakpoint or releasing a pause must not have to wait on whoever
/// is currently blocked in `flow_debug.wait`. The controller is `Sync` and
/// needs no lock; only the session itself — whose `next_pause`/`finish` take
/// `&mut self` — sits behind one, and it is an async lock so a waiting call
/// never blocks the registry.
struct Entry {
    session: Arc<tokio::sync::Mutex<Option<DebugSession>>>,
    controller: Arc<crate::testkit::debug::DebugController>,
    mocks: Arc<MockCaps>,
    tracer: Arc<crate::testkit::trace::RunTracer>,
    last_touched: Instant,
}

/// The JSON-in/JSON-out home for the testkit tools.
///
/// Holds the debug sessions, so a tool call in one turn can pause a run and a
/// tool call in the next can inspect and step it. That is the whole reason this
/// is a stateful object rather than a set of free functions.
///
/// A host constructs one per user (or per workspace) and routes tool calls to
/// [`dispatch`](Self::dispatch).
#[derive(Default)]
pub struct TestkitRegistry {
    sessions: Mutex<HashMap<String, Entry>>,
    runs: Mutex<HashMap<String, RunTrace>>,
}

impl TestkitRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle one tool call.
    ///
    /// `name` is a [`ToolContract::name`](super::ToolContract::name), with any
    /// host prefix already stripped.
    ///
    /// # Errors
    /// Returns a [`ToolError`] with a stable machine-readable
    /// [`code`](ToolError::code) — an unknown tool, a malformed argument, a
    /// session that has expired, a pause that has already been released. Every
    /// one is something the caller can act on rather than a stringly-typed
    /// failure.
    pub async fn dispatch(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        self.reap_idle();
        match name {
            "flow_test.run" => self.run(args).await,
            "flow_test.trace" => self.trace(args),
            "flow_test.node" => self.node(args),
            "flow_debug.start" => self.start(args),
            "flow_debug.breakpoint" => self.breakpoint(args),
            "flow_debug.wait" => self.wait(args).await,
            "flow_debug.status" => self.status(args),
            "flow_debug.release" => self.release(args),
            "flow_debug.trace" => self.session_trace(args),
            "flow_debug.stop" => self.stop(args).await,
            other => Err(ToolError::new(
                ToolErrorCode::UnknownTool,
                format!("no such tool: {other:?}"),
            )),
        }
    }

    /// Drop sessions nobody has touched in a while.
    fn reap_idle(&self) {
        let mut sessions = self.sessions.lock().expect("sessions poisoned");
        sessions.retain(|_, entry| entry.last_touched.elapsed() < SESSION_IDLE_TIMEOUT);
    }

    // ---- flow_test ------------------------------------------------------

    async fn run(&self, args: Value) -> Result<Value, ToolError> {
        let graph = graph_arg(&args)?;
        let mocks = Arc::new(mocks_from(&args)?);
        let input = run_input(&args);
        let compiled = compile(&graph).map_err(|err| {
            ToolError::new(
                ToolErrorCode::InvalidGraph,
                format!("the graph did not compile: {err}"),
            )
        })?;

        let tracer =
            Arc::new(crate::testkit::trace::RunTracer::new(Some(graph)).with_mocks(mocks.clone()));
        let outcome = crate::engine::run_intercepted(
            &compiled,
            input,
            &mocks.capabilities(),
            &(tracer.clone() as Arc<dyn crate::observability::RunObserver>),
            crate::engine::CancellationToken::new(),
            tracer.clone(),
        )
        .await;

        let trace = tracer.trace();
        let run_id = crate::ids::token();
        self.runs
            .lock()
            .expect("runs poisoned")
            .insert(run_id.clone(), trace.clone());

        // A run that failed is still a run worth reporting on: the trace is the
        // most useful thing the caller can be handed, so a node failing is
        // described rather than raised.
        let (status, error) = match &outcome {
            Ok(_) => ("completed", None),
            Err(err) => ("failed", Some(err.to_string())),
        };
        Ok(json!({
            "runId": run_id,
            "status": status,
            "error": error,
            "summary": trace.summary(),
            "diagnosis": trace.diagnosis,
            "nullBindings": null_binding_report(&trace),
            "output": outcome.ok().map(|(outcome, _resumable)| crate::evidence::bounded_evidence(&outcome.output)),
        }))
    }

    fn trace(&self, args: Value) -> Result<Value, ToolError> {
        let run_id = str_arg(&args, "run_id")?;
        let runs = self.runs.lock().expect("runs poisoned");
        let trace = runs.get(&run_id).ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::UnknownRun,
                format!("no run recorded as {run_id:?}"),
            )
        })?;
        Ok(project_trace(
            trace,
            args.get("node_id").and_then(Value::as_str),
        ))
    }

    fn node(&self, args: Value) -> Result<Value, ToolError> {
        let run_id = str_arg(&args, "run_id")?;
        let node_id = str_arg(&args, "node_id")?;
        let runs = self.runs.lock().expect("runs poisoned");
        let trace = runs.get(&run_id).ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::UnknownRun,
                format!("no run recorded as {run_id:?}"),
            )
        })?;
        if !trace.ran(&node_id) {
            // Not an error: a node a condition routed past never ran, and that
            // is usually the answer the caller was looking for.
            return Ok(json!({
                "nodeId": node_id,
                "ran": false,
                "note": "this node never executed — a branch may have routed past it",
            }));
        }
        Ok(json!({
            "nodeId": node_id,
            "ran": true,
            "activations": trace.steps_for(&node_id),
            "calls": trace.calls_from(&node_id),
        }))
    }

    // ---- flow_debug -----------------------------------------------------

    fn start(&self, args: Value) -> Result<Value, ToolError> {
        let graph = graph_arg(&args)?;
        let mocks = Arc::new(mocks_from(&args)?);
        let input = run_input(&args);
        let compiled = compile(&graph).map_err(|err| {
            ToolError::new(
                ToolErrorCode::InvalidGraph,
                format!("the graph did not compile: {err}"),
            )
        })?;

        let tracer =
            Arc::new(crate::testkit::trace::RunTracer::new(Some(graph)).with_mocks(mocks.clone()));
        let session = DebugSession::start(
            compiled,
            input,
            mocks.capabilities(),
            tracer.clone() as Arc<dyn crate::observability::RunObserver>,
        )
        .map_err(|err| ToolError::new(ToolErrorCode::SessionFailed, err.to_string()))?;
        session.controller().use_mocks(mocks.clone());
        let controller = session.controller().clone();

        let session_id = crate::ids::token();
        self.sessions.lock().expect("sessions poisoned").insert(
            session_id.clone(),
            Entry {
                session: Arc::new(tokio::sync::Mutex::new(Some(session))),
                controller,
                mocks,
                tracer,
                last_touched: Instant::now(),
            },
        );
        Ok(json!({
            "sessionId": session_id,
            "note": "set breakpoints with flow_debug.breakpoint, then flow_debug.wait",
        }))
    }

    /// Run `f` against a session, refreshing its idle timer.
    fn with_session<T>(
        &self,
        args: &Value,
        f: impl FnOnce(&mut Entry) -> Result<T, ToolError>,
    ) -> Result<T, ToolError> {
        let session_id = str_arg(args, "session_id")?;
        let mut sessions = self.sessions.lock().expect("sessions poisoned");
        let entry = sessions.get_mut(&session_id).ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::UnknownSession,
                format!("no session {session_id:?}; it may have been reaped after being idle"),
            )
        })?;
        entry.last_touched = Instant::now();
        f(entry)
    }

    fn breakpoint(&self, args: Value) -> Result<Value, ToolError> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("set")
            .to_string();
        self.with_session(&args, |entry| {
            let controller = &entry.controller;
            match action.as_str() {
                "list" => Ok(json!({ "breakpoints": controller.breakpoints() })),
                "clear" => {
                    let id = args
                        .get("breakpoint_id")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| {
                            ToolError::new(
                                ToolErrorCode::InvalidArguments,
                                "clear needs a breakpoint_id".to_string(),
                            )
                        })?;
                    let removed =
                        controller.clear_breakpoint(crate::testkit::debug::BreakpointId(id));
                    Ok(json!({ "cleared": removed }))
                }
                "set" => {
                    let spec = breakpoint_spec(&args)?;
                    let id = controller.set_breakpoint(spec).map_err(|err| {
                        ToolError::new(ToolErrorCode::InvalidArguments, err.to_string())
                    })?;
                    Ok(json!({ "breakpointId": id }))
                }
                other => Err(ToolError::new(
                    ToolErrorCode::InvalidArguments,
                    format!("unknown action {other:?}; expected set, list, or clear"),
                )),
            }
        })
    }

    async fn wait(&self, args: Value) -> Result<Value, ToolError> {
        let timeout = Duration::from_millis(
            args.get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(5_000),
        );
        // The session is taken out from under the lock: awaiting a pause while
        // holding it would block every other tool call on this registry,
        // including the release that would free the run.
        let session_id = str_arg(&args, "session_id")?;
        let handle = {
            let mut sessions = self.sessions.lock().expect("sessions poisoned");
            let entry = sessions.get_mut(&session_id).ok_or_else(|| {
                ToolError::new(
                    ToolErrorCode::UnknownSession,
                    format!("no session {session_id:?}"),
                )
            })?;
            entry.last_touched = Instant::now();
            entry.session.clone()
        };
        let pause = {
            let mut guard = handle.lock().await;
            match guard.as_mut() {
                Some(session) => session.next_pause(timeout).await,
                None => None,
            }
        };

        Ok(match pause {
            Some(pause) => json!({ "paused": true, "pause": pause }),
            None => json!({
                "paused": false,
                "note": "the run finished or nothing broke within the timeout",
            }),
        })
    }

    fn status(&self, args: Value) -> Result<Value, ToolError> {
        self.with_session(&args, |entry| {
            let parked = entry.controller.pauses();
            let status = if !parked.is_empty() {
                "paused"
            } else if entry.controller.is_detached() {
                "detached"
            } else {
                "running"
            };
            Ok(json!({
                "status": status,
                "pauses": parked,
                "breakpoints": entry.controller.breakpoints(),
            }))
        })
    }

    fn release(&self, args: Value) -> Result<Value, ToolError> {
        let pause_id = args
            .get("pause_id")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ToolError::new(
                    ToolErrorCode::InvalidArguments,
                    "release needs a pause_id".to_string(),
                )
            })?;
        let command = debug_command(&args)?;
        self.with_session(&args, |entry| {
            entry
                .controller
                .release(pause_id, command)
                .map_err(|err| ToolError::new(ToolErrorCode::UnknownPause, err.to_string()))?;
            Ok(json!({ "released": pause_id }))
        })
    }

    fn session_trace(&self, args: Value) -> Result<Value, ToolError> {
        let node_id = args
            .get("node_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.with_session(&args, |entry| {
            Ok(project_trace(&entry.tracer.trace(), node_id.as_deref()))
        })
    }

    async fn stop(&self, args: Value) -> Result<Value, ToolError> {
        let session_id = str_arg(&args, "session_id")?;
        let entry = self
            .sessions
            .lock()
            .expect("sessions poisoned")
            .remove(&session_id)
            .ok_or_else(|| {
                ToolError::new(
                    ToolErrorCode::UnknownSession,
                    format!("no session {session_id:?}"),
                )
            })?;
        let trace = entry.tracer.trace();
        let _ = entry.mocks;
        let session = entry.session.lock().await.take();
        let outcome = match session {
            Some(session) => session.finish().await,
            None => Err(crate::error::EngineError::Capability(
                "this session was already stopped".to_string(),
            )),
        };
        Ok(json!({
            "stopped": true,
            "summary": trace.summary(),
            "diagnosis": trace.diagnosis,
            "output": outcome.ok().map(|o| crate::evidence::bounded_evidence(&o.output)),
        }))
    }
}

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

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
