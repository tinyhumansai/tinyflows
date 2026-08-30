//! The front door: build a run, mock what it touches, run it, assert on it.
//!
//! Everything here is available piecemeal — [`MockCaps`], [`RunTracer`] and
//! [`crate::engine::run_intercepted`] compose without it. What this adds is that
//! a test does not have to wire them together, and that the assertions worth
//! making are named rather than left as an exercise.
//!
//! The assertion that matters most is [`TestRun::assert_no_null_bindings`]. A
//! workflow that runs green and does nothing passes every other check: it
//! completed, no node errored, the output is an object. What it did was bind to
//! a field nothing produced and quietly send an empty value onward.
//!
//! ```no_run
//! # async fn example(graph: tinyflows::model::WorkflowGraph) -> tinyflows::error::Result<()> {
//! use tinyflows::testkit::{Respond, TestHarness};
//! use serde_json::json;
//!
//! let run = TestHarness::new(&graph)
//!     .trigger(json!({ "repo": "acme/api" }))
//!     .input("branch", json!("main"))
//!     .mock_tool("slack.send", Respond::value(json!({ "ok": true })))
//!     .run()
//!     .await?;
//!
//! run.assert_completed();
//! run.assert_node_ran("send_email");
//! run.assert_no_null_bindings();
//! run.assert_call_count("tools", Some("slack.send"), 1);
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::compiler::compile;
use crate::engine::{CancellationToken, RunInput, RunOutcome, run_intercepted};
use crate::error::Result;
use crate::model::WorkflowGraph;
use crate::observability::RunObserver;

use super::mocks::{MockCaps, Respond};
use super::trace::{RunTrace, RunTracer, TraceStatus};

/// Builds a mocked, traced run of one workflow.
pub struct TestHarness {
    graph: WorkflowGraph,
    input: RunInput,
    mocks: MockCaps,
}

impl TestHarness {
    /// A harness for `graph`, with every capability mocked and every call
    /// traced.
    #[must_use]
    pub fn new(graph: &WorkflowGraph) -> Self {
        Self {
            graph: graph.clone(),
            input: RunInput::new(Value::Null),
            mocks: MockCaps::new(),
        }
    }

    /// Set the trigger payload.
    #[must_use]
    pub fn trigger(mut self, trigger: Value) -> Self {
        self.input.trigger = trigger;
        self
    }

    /// Supply one of the workflow's declared inputs.
    #[must_use]
    pub fn input(mut self, name: &str, value: Value) -> Self {
        self.input.inputs.insert(name.to_string(), value);
        self
    }

    /// Supply every declared input at once.
    #[must_use]
    pub fn inputs(mut self, inputs: Map<String, Value>) -> Self {
        self.input.inputs = inputs;
        self
    }

    /// Pre-approve a human-in-the-loop gate, so the run does not park on it.
    #[must_use]
    pub fn approve(mut self, node_id: &str) -> Self {
        self.input.approvals.push(node_id.to_string());
        self
    }

    /// Program a tool response. `slug` may contain `*`.
    #[must_use]
    pub fn mock_tool(mut self, slug: &str, respond: Respond) -> Self {
        self.mocks = std::mem::take(&mut self.mocks).on_tool(slug, respond);
        self
    }

    /// Program an HTTP response. `url` may contain `*`.
    #[must_use]
    pub fn mock_http(mut self, url: &str, respond: Respond) -> Self {
        self.mocks = std::mem::take(&mut self.mocks).on_http(url, respond);
        self
    }

    /// Program the LLM provider.
    #[must_use]
    pub fn mock_llm(mut self, respond: Respond) -> Self {
        self.mocks = std::mem::take(&mut self.mocks).on_llm(respond);
        self
    }

    /// Program the agent runner for `agent_ref`, which may contain `*`.
    #[must_use]
    pub fn mock_agent(mut self, agent_ref: &str, respond: Respond) -> Self {
        self.mocks = std::mem::take(&mut self.mocks).on_agent(agent_ref, respond);
        self
    }

    /// Restrict the most recently programmed mock to one node.
    #[must_use]
    pub fn only_from(mut self, node_id: &str) -> Self {
        self.mocks = std::mem::take(&mut self.mocks).only_from(node_id);
        self
    }

    /// Register a graph a `sub_workflow` node can resolve by id.
    #[must_use]
    pub fn with_workflow(mut self, id: impl Into<String>, graph: WorkflowGraph) -> Self {
        self.mocks = std::mem::take(&mut self.mocks).with_workflow(id, graph);
        self
    }

    /// Compile and run.
    ///
    /// # Errors
    /// Returns the compile error for an invalid graph, or the run's own error
    /// when a node fails under a `stop` policy. A run that *completed* with
    /// failures inside it is not an error here — that is what
    /// [`TestRun::assert_completed`] is for.
    pub async fn run(self) -> Result<TestRun> {
        let compiled = compile(&self.graph)?;
        let mocks = Arc::new(self.mocks);
        let tracer = Arc::new(RunTracer::new(Some(self.graph.clone())).with_mocks(mocks.clone()));
        let (outcome, _resumable) = run_intercepted(
            &compiled,
            self.input,
            &mocks.capabilities(),
            &(tracer.clone() as Arc<dyn RunObserver>),
            CancellationToken::new(),
            tracer.clone(),
        )
        .await?;
        Ok(TestRun {
            outcome,
            trace: tracer.trace(),
            mocks,
        })
    }
}

/// A finished run, with everything needed to interrogate it.
pub struct TestRun {
    outcome: RunOutcome,
    trace: RunTrace,
    mocks: Arc<MockCaps>,
}

impl TestRun {
    /// The engine's own outcome.
    #[must_use]
    pub fn outcome(&self) -> &RunOutcome {
        &self.outcome
    }

    /// The final run state.
    #[must_use]
    pub fn output(&self) -> &Value {
        &self.outcome.output
    }

    /// The structured trace.
    #[must_use]
    pub fn trace(&self) -> &RunTrace {
        &self.trace
    }

    /// The mocks the run used, for reading the raw call log.
    #[must_use]
    pub fn mocks(&self) -> &Arc<MockCaps> {
        &self.mocks
    }

    /// The items `node_id` emitted, as JSON.
    #[must_use]
    pub fn node_output(&self, node_id: &str) -> Vec<Value> {
        self.trace
            .steps_for(node_id)
            .iter()
            .flat_map(|step| step.output.iter().map(|item| item.json.clone()))
            .collect()
    }

    /// Panic unless the run finished with no node failing and nothing pending.
    ///
    /// # Panics
    /// If any node failed, or the run parked on an approval gate.
    pub fn assert_completed(&self) {
        let failed = self.trace.failed();
        assert!(
            failed.is_empty(),
            "expected a clean run, but these nodes failed: {:?}",
            failed
                .iter()
                .map(|s| (&s.node_id, &s.error))
                .collect::<Vec<_>>()
        );
        assert!(
            self.outcome.pending_approvals.is_empty(),
            "the run parked awaiting approval of {:?}; pre-approve with `.approve(..)`",
            self.outcome.pending_approvals
        );
    }

    /// Panic unless `node_id` executed at least once.
    ///
    /// Worth asserting explicitly: a node a condition routed past leaves no
    /// step, and a graph half of which never ran still reports a clean outcome.
    ///
    /// # Panics
    /// If the node never ran.
    pub fn assert_node_ran(&self, node_id: &str) {
        assert!(
            self.trace.ran(node_id),
            "node {node_id:?} never ran; nodes that did: {:?}",
            self.trace
                .steps
                .iter()
                .map(|s| &s.node_id)
                .collect::<Vec<_>>()
        );
    }

    /// Panic if `node_id` executed.
    ///
    /// # Panics
    /// If the node ran.
    pub fn assert_node_skipped(&self, node_id: &str) {
        assert!(
            !self.trace.ran(node_id),
            "expected node {node_id:?} not to run, but it did"
        );
    }

    /// Panic unless `node_id` failed.
    ///
    /// # Panics
    /// If the node did not fail.
    pub fn assert_node_failed(&self, node_id: &str) {
        assert!(
            self.trace
                .steps_for(node_id)
                .iter()
                .any(|s| s.status == TraceStatus::Error),
            "expected node {node_id:?} to fail, but it did not"
        );
    }

    /// Panic if any `=`-binding resolved to nothing.
    ///
    /// The check a green run hides. A binding that resolved to `null` is not an
    /// error to the engine — the node ran, the field was empty, and the
    /// workflow did nothing.
    ///
    /// # Panics
    /// If any binding resolved to null, naming each one and the upstream node
    /// it was reading from.
    pub fn assert_no_null_bindings(&self) {
        let nulls = self.trace.null_bindings();
        assert!(
            nulls.is_empty(),
            "these bindings resolved to null:\n{}",
            nulls
                .iter()
                .map(|(node, b)| format!(
                    "  {node}.{} = {:?}{}",
                    b.location,
                    b.expression,
                    match &b.reads_from {
                        Some(from) => format!(" (reads from node {from:?})"),
                        None => String::new(),
                    }
                ))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// Panic unless exactly `expected` matching capability calls were made.
    ///
    /// `capability` is one of the [`super::capability`] constants; `target`
    /// accepts the same `*` globbing the mocks do.
    ///
    /// # Panics
    /// If the count differs.
    pub fn assert_call_count(&self, capability: &str, target: Option<&str>, expected: usize) {
        let actual = self.mocks.log().count(capability, target);
        assert_eq!(
            actual,
            expected,
            "expected {expected} {capability} call(s) to {}, saw {actual}",
            target.unwrap_or("*")
        );
    }

    /// Panic unless the run's diagnosis is clean.
    ///
    /// Stricter than [`assert_no_null_bindings`](Self::assert_no_null_bindings):
    /// this also catches agent nodes that would dispatch with an empty prompt
    /// and failures an `on_error` policy swallowed.
    ///
    /// # Panics
    /// If the diagnosis reports anything actionable.
    pub fn assert_clean_diagnosis(&self) {
        assert!(
            self.trace.diagnosis.is_clean(),
            "the run's diagnosis is not clean: {:#?}",
            self.trace.diagnosis
        );
    }
}

#[cfg(test)]
#[path = "harness_tests.rs"]
mod tests;
