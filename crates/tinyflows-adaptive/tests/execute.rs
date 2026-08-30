//! Execute, against the real engine.
//!
//! Not a mocked engine: these compile and run actual graphs, because the whole
//! point of the layer is the gap between what the engine returns and what the
//! judge needs, and a mock of the engine would be a mock of exactly that gap.

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};
use tinyflows_adaptive::contracts::Approach;
use tinyflows_adaptive::execute::{Unobserved, Workspace, run_attempt};
use tinyflows_adaptive::intake::Attempt;

/// Records whether the baseline was taken before the run.
#[derive(Default)]
struct Recording {
    calls: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl Workspace for Recording {
    async fn mark(&self) -> String {
        self.calls.lock().expect("lock").push("mark".into());
        "baseline-7".into()
    }
    async fn changed_since(&self, mark: &str) -> String {
        self.calls
            .lock()
            .expect("lock")
            .push(format!("changed_since({mark})"));
        "wrote report.md".into()
    }
}

fn node(id: &str, kind: NodeKind, config: Value) -> Node {
    Node {
        id: id.into(),
        kind,
        type_version: 1,
        name: id.into(),
        config,
        ports: Vec::new(),
        position: None,
    }
}

fn edge(from: &str, to: &str) -> Edge {
    Edge {
        from_node: from.into(),
        from_port: "main".into(),
        to_node: to.into(),
        to_port: "main".into(),
    }
}

fn graph(nodes: Vec<Node>, edges: Vec<Edge>) -> WorkflowGraph {
    WorkflowGraph {
        schema_version: 1,
        id: Some("t".into()),
        name: "t".into(),
        inputs: Vec::new(),
        agents: Vec::new(),
        nodes,
        edges,
    }
}

fn attempt(graph: WorkflowGraph) -> Attempt {
    Attempt {
        resume: None,
        approach: Approach::Authored {
            why: "for the test".into(),
            fingerprint: "0000000".into(),
        },
        graph,
        inputs: Map::new(),
        lessons_shown: Vec::new(),
    }
}

/// One trigger into one transform: the smallest graph that actually does work.
fn working() -> WorkflowGraph {
    graph(
        vec![
            node(
                "start",
                NodeKind::Trigger,
                json!({"trigger_kind": "manual"}),
            ),
            node("done", NodeKind::Transform, json!({"set": {"ok": true}})),
        ],
        vec![edge("start", "done")],
    )
}

#[tokio::test]
async fn a_clean_run_comes_back_with_a_clean_diagnosis_and_the_host_s_reading() {
    let workspace = Recording::default();
    let ran = run_attempt(&attempt(working()), &mock_capabilities(), &workspace).await;

    assert!(ran.failed.is_none(), "{:?}", ran.failed);
    assert_eq!(ran.changed, "wrote report.md");
    assert!(
        ran.diagnosis.never_ran.is_empty(),
        "both nodes ran: {:?}",
        ran.diagnosis.never_ran
    );

    // The ordering the trait exists for: a baseline, then the run, then the
    // comparison against that same baseline.
    let calls = workspace.calls.lock().expect("lock").clone();
    assert_eq!(calls, vec!["mark", "changed_since(baseline-7)"]);
}

#[tokio::test]
async fn the_diagnosis_is_populated_which_is_the_reason_an_observer_is_attached() {
    // A condition that routes past a node. `RunOutcome` alone cannot say this
    // happened — the run is green either way — and every downstream gate reads
    // `never_ran` to find out.
    let g = graph(
        vec![
            node(
                "start",
                NodeKind::Trigger,
                json!({"trigger_kind": "manual"}),
            ),
            node(
                "gate",
                NodeKind::Condition,
                json!({"conditions": [{"left": "=item.nope", "operator": "equals", "right": "yes"}]}),
            ),
            // An `http_request`, not a transform: `never_ran` deliberately
            // reports only the kinds that do outside work, because a routed-past
            // transform is not a surprise worth warning about.
            node(
                "skipped",
                NodeKind::HttpRequest,
                json!({"url": "https://example.invalid/report", "method": "GET"}),
            ),
        ],
        vec![
            edge("start", "gate"),
            Edge {
                from_node: "gate".into(),
                from_port: "true".into(),
                to_node: "skipped".into(),
                to_port: "main".into(),
            },
        ],
    );

    let ran = run_attempt(&attempt(g), &mock_capabilities(), &Unobserved).await;

    assert!(
        ran.failed.is_none(),
        "the run itself is fine: {:?}",
        ran.failed
    );
    assert!(
        ran.diagnosis
            .never_ran
            .iter()
            .any(|n| n.node_id == "skipped"),
        "a blank diagnosis here would mean nobody looked: {:?}",
        ran.diagnosis
    );
}

#[tokio::test]
async fn a_graph_that_does_not_compile_is_an_attempt_not_an_error() {
    // No trigger node. Intake would never return this, but a caller that hand-
    // builds an `Attempt` can, and it still has to leave a ledger row.
    let g = graph(
        vec![node(
            "lonely",
            NodeKind::Transform,
            json!({"set": {"ok": true}}),
        )],
        Vec::new(),
    );

    let ran = run_attempt(&attempt(g), &mock_capabilities(), &Unobserved).await;

    let failure = ran.failed.expect("it did not compile");
    assert!(!failure.is_empty());
    // Readable through the ordinary evidence path, with no special case.
    assert_eq!(ran.outcome.output["error"], json!(failure));
    // And no `nodes` key, so the mechanical missing-evidence check fires.
    assert!(ran.outcome.output.get("nodes").is_none());
}

#[tokio::test]
async fn a_silent_host_is_silent_rather_than_wrong() {
    let ran = run_attempt(&attempt(working()), &mock_capabilities(), &Unobserved).await;
    assert!(ran.changed.is_empty());
    assert!(ran.failed.is_none());
    // Empty reads as "nothing reported", never as "nothing happened".
    assert!(ran.evidence().changed.is_empty());
}

#[tokio::test]
async fn the_evidence_borrows_what_ran_owns() {
    let ran = run_attempt(&attempt(working()), &mock_capabilities(), &Unobserved).await;
    let evidence = ran.evidence();
    assert!(std::ptr::eq(evidence.outcome, &ran.outcome));
    assert!(std::ptr::eq(evidence.diagnosis, &ran.diagnosis));
}

// ---------------------------------------------------------------------------
// The port: local and remote must be indistinguishable to the loop.
// ---------------------------------------------------------------------------

use tinyflows_adaptive::execute::{Local, Relay, Remote, RunReport, RunRequest, Runner, serve};

/// A relay that actually serializes, so the round trip is the real one.
struct Loopback {
    seen: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl Relay for Loopback {
    async fn dispatch(&self, request: &RunRequest) -> Result<RunReport, String> {
        // Out over a wire...
        let wire = serde_json::to_string(request).expect("request serializes");
        self.seen.lock().expect("lock").push(wire.clone());
        let received: RunRequest = serde_json::from_str(&wire).expect("request deserializes");

        // ...run on the far side, exactly as a device would...
        let report = serve(&received, &mock_capabilities(), &Unobserved).await;

        // ...and back.
        let wire = serde_json::to_string(&report).expect("report serializes");
        Ok(serde_json::from_str(&wire).expect("report deserializes"))
    }
}

/// A model that answers once from a script and counts the asking.
struct Scripted {
    replies: std::sync::Mutex<Vec<Value>>,
    calls: std::sync::Mutex<usize>,
}

impl Scripted {
    fn new(replies: Vec<Value>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            replies: std::sync::Mutex::new(replies),
            calls: std::sync::Mutex::new(0),
        })
    }
    fn call_count(&self) -> usize {
        *self.calls.lock().expect("lock")
    }
}

#[async_trait]
impl tinyflows::caps::LlmProvider for Scripted {
    async fn complete(
        &self,
        _request: Value,
        _conn: Option<&str>,
    ) -> tinyflows::error::Result<Value> {
        *self.calls.lock().expect("lock") += 1;
        let mut replies = self.replies.lock().expect("lock");
        assert!(
            !replies.is_empty(),
            "asked more times than the script answers"
        );
        Ok(replies.remove(0))
    }
}

fn caps_with(llm: std::sync::Arc<Scripted>) -> tinyflows::caps::Capabilities {
    tinyflows::caps::Capabilities {
        llm,
        ..mock_capabilities()
    }
}

struct Dead(&'static str);

#[async_trait]
impl Relay for Dead {
    async fn dispatch(&self, _request: &RunRequest) -> Result<RunReport, String> {
        Err(self.0.to_string())
    }
}

#[tokio::test]
async fn a_run_relayed_over_a_wire_judges_the_same_as_one_run_in_process() {
    // The property the whole port rests on: the loop cannot tell the
    // difference, because both paths are serve() + into_ran() with only a
    // serialization boundary between them.
    let a = attempt(working());
    let caps = mock_capabilities();

    let here = Local {
        caps: &caps,
        workspace: &Unobserved,
    }
    .run(&a)
    .await;

    let relay = Loopback {
        seen: std::sync::Mutex::new(Vec::new()),
    };
    let there = Remote {
        relay: &relay,
        attempt_id: "ep-1/1".into(),
    }
    .run(&a)
    .await;

    assert_eq!(here.outcome.output, there.outcome.output);
    assert_eq!(here.diagnosis, there.diagnosis);
    assert_eq!(here.failed, there.failed);
    assert_eq!(here.steps.len(), there.steps.len());
    assert_eq!(here.changed, there.changed);
}

#[tokio::test]
async fn the_graph_crosses_and_the_history_does_not() {
    let relay = Loopback {
        seen: std::sync::Mutex::new(Vec::new()),
    };
    Remote {
        relay: &relay,
        attempt_id: "ep-9/2".into(),
    }
    .run(&attempt(working()))
    .await;

    let sent = relay.seen.lock().expect("lock")[0].clone();
    assert!(sent.contains("attemptId"), "correlation: {sent}");
    assert!(sent.contains("\"nodes\""), "the graph itself crosses");
    // A runner sees one graph and nothing about the episode it belongs to.
    for leak in ["episode", "lesson", "ledger", "approachSig", "verdict"] {
        assert!(!sent.contains(leak), "`{leak}` must not cross: {sent}");
    }
}

#[tokio::test]
async fn a_runner_that_never_answers_still_produces_a_judgeable_attempt() {
    let ran = Remote {
        relay: &Dead("deadline elapsed after 600s"),
        attempt_id: "ep-2/4".into(),
    }
    .run(&attempt(working()))
    .await;

    assert!(ran.failed.is_some());
    assert!(ran.steps.is_empty());
    // And crucially: it does not claim nothing changed, because nobody looked.
    // Claiming it would settle the verdict as MissingEvidence, which is
    // terminal — ending the episode because a socket blipped.
    assert!(!ran.changed.is_empty(), "{}", ran.changed);
}

#[tokio::test]
async fn an_unanswered_run_is_judged_rather_than_settled_terminally() {
    // The end-to-end version of the above: the judge is asked, which is only
    // possible because `changed` is not empty. A model that is never called
    // panics here, proving the mechanical path did not swallow it.
    use tinyflows_adaptive::closing::judge;
    use tinyflows_adaptive::contracts::{Blocker, Goal};

    let ran = Remote {
        relay: &Dead("no runner connected"),
        attempt_id: "ep-3/1".into(),
    }
    .run(&attempt(working()))
    .await;

    let llm = Scripted::new(vec![json!({
        "satisfied": false,
        "blocker": "goal_not_met",
        "gap": "the runner never reported, so nothing is established",
        "advanced": false
    })]);
    let verdict = judge(
        &Goal::new("write the weekly report"),
        &ran.evidence(),
        &caps_with(llm.clone()),
        None,
    )
    .await
    .expect("judged");

    assert_eq!(llm.call_count(), 1, "it reached the judge");
    assert_eq!(verdict.blocker, Blocker::GoalNotMet);
    assert!(verdict.blocker.continuable(), "the episode can still retry");
}
