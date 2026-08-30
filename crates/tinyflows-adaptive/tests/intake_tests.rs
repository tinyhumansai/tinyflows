//! Intake, end to end, against a scripted model and a real store.
//!
//! The unit tests cover rendering and parsing. These cover the decision: that
//! selection is preferred, that authoring is the fallback rather than the
//! default, and that the exclusion list actually excludes — which is the
//! property the whole retry edge rests on and the one that is invisible until
//! an episode has spent an attempt.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::caps::{Capabilities, LlmProvider};
use tinyflows::error::Result as EngineResult;
use tinyflows::model::{Edge, InputType, Node, NodeKind, WorkflowGraph, WorkflowInput};
use tinyflows::store::types::WorkflowRecord;
use tinyflows::store::{FileWorkflowStore, WorkflowStore};
use tinyflows_adaptive::contracts::{Approach, Goal};
use tinyflows_adaptive::host::HostFacts;
use tinyflows_adaptive::intake::decide;
use tinyflows_adaptive::ledger::{Ledger, memory::MemoryLedger};

/// A provider that answers from a script and records what it was asked.
struct Scripted {
    replies: Mutex<Vec<Value>>,
    seen: Mutex<Vec<String>>,
}

impl Scripted {
    fn new(replies: Vec<Value>) -> Self {
        Self {
            replies: Mutex::new(replies),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn prompts(&self) -> Vec<String> {
        self.seen.lock().expect("lock").clone()
    }
}

#[async_trait]
impl LlmProvider for Scripted {
    async fn complete(&self, request: Value, _conn: Option<&str>) -> EngineResult<Value> {
        let text = request["messages"]
            .as_array()
            .map(|m| {
                m.iter()
                    .filter_map(|msg| msg["content"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        self.seen.lock().expect("lock").push(text);

        let mut replies = self.replies.lock().expect("lock");
        if replies.is_empty() {
            panic!("the model was asked more times than the script has answers");
        }
        Ok(replies.remove(0))
    }
}

/// The engine's mock bundle with only the model replaced: nothing in intake
/// touches tools, HTTP, code or state, so scripting those too would be noise.
fn caps_with(llm: std::sync::Arc<Scripted>) -> Capabilities {
    Capabilities {
        llm,
        ..mock_capabilities()
    }
}

/// A store on a fresh temp directory, so each case starts empty.
fn empty_store(tag: &str) -> (FileWorkflowStore, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("adaptive-intake-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("workflows")).expect("temp dir");
    let store = FileWorkflowStore::new(vec![root.join("workflows")], root.join("runs"));
    (store, root)
}

/// A minimal graph that validates: one trigger, one transform.
/// An author reply in the recipe surface, with `label` distinguishing its
/// lowered shape (and so its fingerprint) from any other reply's.
fn authored_reply(label: &str, required_input: Option<&str>) -> Value {
    let mut reply = json!({
        "why": label,
        "inputs": {},
        "steps": [{ "id": "work", "ask": format!("Do the {label} work directly.") }],
    });
    if let Some(name) = required_input {
        reply["declared"] = json!([{ "name": name, "description": "", "required": true }]);
        reply["inputs"] = json!({ name: "acme/thing" });
    }
    reply
}

fn tiny_graph(name: &str, required_input: Option<&str>) -> WorkflowGraph {
    WorkflowGraph {
        schema_version: 1,
        id: Some(name.to_string()),
        name: name.to_string(),
        inputs: required_input
            .map(|n| vec![WorkflowInput::new(n, InputType::String).required()])
            .unwrap_or_default(),
        agents: Vec::new(),
        nodes: vec![
            Node {
                id: "start".into(),
                kind: NodeKind::Trigger,
                type_version: 1,
                name: "manual".into(),
                config: json!({ "trigger_kind": "manual" }),
                ports: Vec::new(),
                position: None,
            },
            Node {
                id: "done".into(),
                kind: NodeKind::Transform,
                type_version: 1,
                name: "done".into(),
                config: json!({ "set": { "ok": true } }),
                ports: Vec::new(),
                position: None,
            },
        ],
        edges: vec![Edge {
            from_node: "start".into(),
            from_port: "main".into(),
            to_node: "done".into(),
            to_port: "main".into(),
        }],
    }
}

fn stored(id: &str, description: &str, required_input: Option<&str>) -> WorkflowRecord {
    WorkflowRecord {
        id: id.to_string(),
        name: id.to_string(),
        description: description.to_string(),
        enabled: true,
        defaults: Default::default(),
        graph: tiny_graph(id, required_input),
        source_path: None,
    }
}

/// A selection call that declines, scripted ahead of an authoring reply.
///
/// Needed since the errand triage landed: `select` now has a third answer, so
/// it is asked even when the shelf is empty — the old short-circuit assumed the
/// answer could only be "none", and that stopped being true. Written into each
/// script rather than defaulted inside the harness, so a test still shows every
/// call its path makes instead of hiding one behind a lenient double.
fn select_declines() -> Value {
    json!({ "workflow_id": null, "errand": false, "why": "nothing stored fits" })
}

/// The authoring prompt, found by what it says rather than by its position.
///
/// Positional indexing broke the moment a call was added in front of it, and
/// would break again; the authoring system prompt is self-identifying.
fn authoring_prompt(llm: &std::sync::Arc<Scripted>) -> String {
    llm.prompts()
        .into_iter()
        .find(|p| p.contains("You plan how to achieve a goal"))
        .expect("the author was asked")
}

#[tokio::test]
async fn an_empty_store_authors_without_asking_whether_to_select() {
    // With nothing to choose from the answer can only be "none". Spending a
    // call to be told so is the cost of every cold start.
    let llm = std::sync::Arc::new(Scripted::new(vec![
        select_declines(),
        authored_reply("fresh", None),
    ]));
    let caps = caps_with(llm.clone());
    let (store, _root) = empty_store("1");
    let ledger = MemoryLedger::new();

    let attempt = decide(
        &Goal::new("do a new thing"),
        "ep1",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect("decide");

    assert!(matches!(attempt.approach, Approach::Authored { .. }));
    // Two calls, and the first one is new: a triage that costs a small call to
    // ask whether this is an errand at all. It used to be one, on the reasoning
    // that with nothing to choose from the answer could only be "none" — true
    // until `select` gained a third answer. The trade is deliberate: a cold
    // shelf is exactly where a trivial goal would otherwise pay the full
    // authoring call and get a one-step graph filed for it.
    assert_eq!(llm.prompts().len(), 2, "a triage call, then authoring");
    assert!(
        llm.prompts()[0].contains("whether a saved workflow already does"),
        "the first call is the triage"
    );
    assert!(
        authoring_prompt(&llm).contains("You plan how to achieve a goal"),
        "authoring must speak the recipe surface, not graph syntax"
    );
}

#[tokio::test]
async fn a_matching_workflow_is_selected_and_its_graph_is_loaded() {
    let llm = std::sync::Arc::new(Scripted::new(vec![json!({
        "workflow_id": "pr-review",
        "why": "does exactly this",
        "inputs": {},
    })]));
    let caps = caps_with(llm.clone());
    let (store, _root) = empty_store("2");
    store
        .save(&stored("pr-review", "reviews a closed issue", None))
        .expect("save");
    let ledger = MemoryLedger::new();

    let attempt = decide(
        &Goal::new("review a closed issue"),
        "ep1",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect("decide");

    match attempt.approach {
        Approach::Selected { workflow_id, .. } => assert_eq!(workflow_id, "pr-review"),
        other => panic!("expected a selection, got {other:?}"),
    }
    // The bug this catches: `select` answers with an id, and returning that
    // unbound hands the engine an empty graph that compiles to nothing.
    assert_eq!(
        attempt.graph.nodes.len(),
        2,
        "the stored graph must be loaded"
    );
    assert_eq!(llm.prompts().len(), 1, "a hit must not also author");
}

#[tokio::test]
async fn declining_falls_through_to_authoring() {
    let llm = std::sync::Arc::new(Scripted::new(vec![
        json!({ "workflow_id": null, "why": "none of these fetch anything" }),
        authored_reply("written", None),
    ]));
    let caps = caps_with(llm.clone());
    let (store, _root) = empty_store("3");
    store
        .save(&stored("unrelated", "does something else", None))
        .expect("save");
    let ledger = MemoryLedger::new();

    let attempt = decide(
        &Goal::new("something new"),
        "ep1",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect("decide");

    assert!(matches!(attempt.approach, Approach::Authored { .. }));
    assert_eq!(
        llm.prompts().len(),
        2,
        "selection was asked first, then authoring"
    );
}

#[tokio::test]
async fn a_workflow_already_tried_this_episode_is_not_offered_again() {
    // The property the whole retry edge rests on. Without it attempt two
    // re-selects what attempt one already failed on, and the episode pays
    // twice for one dead end.
    let llm = std::sync::Arc::new(Scripted::new(vec![
        select_declines(),
        authored_reply("written", None),
    ]));
    let caps = caps_with(llm.clone());
    let (store, _root) = empty_store("4");
    store
        .save(&stored("pr-review", "reviews a closed issue", None))
        .expect("save");

    let ledger = MemoryLedger::new();
    let mut spent = tinyflows_adaptive::ledger::conformance::row("ep1", 1, "selected:pr-review");
    spent.workflow_id = Some("pr-review".to_string());
    ledger.append(&spent).await.expect("append");

    let attempt = decide(
        &Goal::new("review a closed issue"),
        "ep1",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect("decide");

    assert!(
        matches!(attempt.approach, Approach::Authored { .. }),
        "the only stored workflow was excluded, so authoring is the only path left"
    );
    // The triage still runs — a goal can be an errand whatever the shelf holds
    // — but the excluded workflow must not appear in front of it. Asserting on
    // what the chooser was *shown* is the claim this test is named for;
    // asserting the call never happened only ever stood in for it.
    // Precisely: absent from the *shelf*. It still appears further down, in the
    // rendered history — that is the exclusion list doing its job, and asserting
    // the id is absent altogether would forbid the very thing that tells the
    // planner not to repeat it.
    let shown = &llm.prompts()[0];
    assert!(
        shown.contains("(none yet"),
        "with every candidate excluded the shelf is empty: {shown}"
    );
    assert!(
        shown.contains("[selected:pr-review]"),
        "and the history still says what was tried: {shown}"
    );
}

#[tokio::test]
async fn a_selection_missing_an_input_gets_the_refusal_back_and_binds_on_the_retry() {
    // The model is confident about inputs it did not find in the goal. The
    // cheap deterministic check catches what the expensive one asserted —
    // and hands it back, because the slip is correctable and ending the
    // episode over it would waste a sound selection.
    let llm = std::sync::Arc::new(Scripted::new(vec![
        json!({ "workflow_id": "needs-repo", "why": "matches", "inputs": {} }),
        json!({
            "workflow_id": "needs-repo",
            "why": "matches, with the input this time",
            "inputs": { "repo": "acme/thing" },
        }),
    ]));
    let caps = caps_with(llm.clone());
    let (store, _root) = empty_store("5");
    store
        .save(&stored("needs-repo", "reviews PRs in a repo", Some("repo")))
        .expect("save");
    let ledger = MemoryLedger::new();

    let attempt = decide(
        &Goal::new("review the PRs on acme/thing"),
        "ep1",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect("the retried selection binds");

    assert!(matches!(attempt.approach, Approach::Selected { .. }));
    assert_eq!(attempt.inputs["repo"], "acme/thing");
    let retry_prompt = llm.prompts().pop().expect("two prompts");
    assert!(
        retry_prompt.contains("failed to bind") && retry_prompt.contains("repo"),
        "the retry names the refusal and the input: {retry_prompt}"
    );
}

#[tokio::test]
async fn a_selection_that_still_cannot_bind_falls_back_to_authoring() {
    // Two unbindable selections mean the goal does not carry what the
    // workflow needs — authoring is the planner that can always produce
    // something runnable, and it sees the refusal too.
    let llm = std::sync::Arc::new(Scripted::new(vec![
        json!({ "workflow_id": "needs-repo", "why": "matches", "inputs": {} }),
        json!({ "workflow_id": "needs-repo", "why": "still sure", "inputs": {} }),
        authored_reply("fresh", None),
    ]));
    let caps = caps_with(llm.clone());
    let (store, _root) = empty_store("5b");
    store
        .save(&stored("needs-repo", "reviews PRs in a repo", Some("repo")))
        .expect("save");
    let ledger = MemoryLedger::new();

    let attempt = decide(
        &Goal::new("review the PRs"),
        "ep1",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect("authoring takes over");

    assert!(matches!(attempt.approach, Approach::Authored { .. }));
    let author_prompt = llm.prompts().pop().expect("three prompts");
    assert!(
        author_prompt.contains("failed to bind"),
        "the author sees why selection was abandoned: {author_prompt}"
    );
}

#[tokio::test]
async fn inputs_the_graph_never_declared_are_trimmed_before_the_engine_sees_them() {
    // The engine rejects undeclared keys before any node executes, so one
    // invented input — models invent them freely — would turn a sound
    // selection into an attempt that ran nothing.
    let llm = std::sync::Arc::new(Scripted::new(vec![json!({
        "workflow_id": "needs-repo",
        "why": "matches",
        "inputs": { "repo": "acme/thing", "topic": "invented", "verbosity": "high" },
    })]));
    let caps = caps_with(llm);
    let (store, _root) = empty_store("trim");
    store
        .save(&stored("needs-repo", "reviews PRs in a repo", Some("repo")))
        .expect("save");
    let ledger = MemoryLedger::new();

    let attempt = decide(
        &Goal::new("review the PRs on acme/thing"),
        "ep1",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect("a sound selection with over-supplied inputs must still bind");

    assert_eq!(
        attempt.inputs.keys().collect::<Vec<_>>(),
        ["repo"],
        "only the declared input survives"
    );
}

#[tokio::test]
async fn a_hallucinated_workflow_id_reads_as_a_decline() {
    let llm = std::sync::Arc::new(Scripted::new(vec![
        json!({ "workflow_id": "pr-reviewer", "why": "close, but no such id" }),
        authored_reply("written", None),
    ]));
    let caps = caps_with(llm.clone());
    let (store, _root) = empty_store("6");
    store
        .save(&stored("pr-review", "reviews a closed issue", None))
        .expect("save");
    let ledger = MemoryLedger::new();

    let attempt = decide(
        &Goal::new("review something"),
        "ep1",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect("decide");

    assert!(
        matches!(attempt.approach, Approach::Authored { .. }),
        "a name that is not on the list is a hallucination, not a lookup"
    );
}

include!("intake/intake_part_01_tests.rs");
include!("intake/intake_part_02_tests.rs");
