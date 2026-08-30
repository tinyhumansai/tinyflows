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

#[tokio::test]
async fn an_authored_graph_that_does_not_validate_is_an_error_not_a_return_value() {
    // Handing it back would turn an authoring mistake into a run-time failure
    // that reads like the work failing. The author retries with the refusal
    // fed back, so the script holds a model that stays wrong for every round.
    let broken = json!({
        "why": "forgot the steps",
        "inputs": {},
    });
    let llm = std::sync::Arc::new(Scripted::new(vec![
        select_declines(),
        broken.clone(),
        broken.clone(),
        broken,
    ]));
    let caps = caps_with(llm);
    let (store, _root) = empty_store("7");
    let ledger = MemoryLedger::new();

    let err = decide(
        &Goal::new("anything"),
        "ep1",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect_err("an invalid graph must not leave intake");
    assert!(err.to_string().contains("invalid"), "{err}");
}

#[tokio::test]
async fn a_disabled_workflow_is_never_offered() {
    let llm = std::sync::Arc::new(Scripted::new(vec![
        select_declines(),
        authored_reply("written", None),
    ]));
    let caps = caps_with(llm.clone());
    let (store, _root) = empty_store("8");
    let mut off = stored("switched-off", "would have matched", None);
    off.enabled = false;
    store.save(&off).expect("save");
    let ledger = MemoryLedger::new();

    decide(
        &Goal::new("do the thing"),
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
        !llm.prompts()[0].contains("switched-off"),
        "offering a disabled workflow invites a choice that cannot be honoured: {}",
        llm.prompts()[0]
    );
}

#[tokio::test]
async fn a_graph_naming_a_worker_this_host_lacks_is_refused_before_it_runs() {
    // The whole point of collecting host facts. Without this the graph saves
    // cleanly, validates cleanly, and fails at run time — usually overnight,
    // to nobody watching.
    //
    // Three copies: the author feeds refusals back, and this model never
    // learns that the worker does not exist.
    let insistent = json!({
        "why": "needs an agent",
        "inputs": {},
        "steps": [{ "id": "work", "ask": "do the thing", "worker": "desktop" }],
    });
    let llm = std::sync::Arc::new(Scripted::new(vec![
        select_declines(),
        insistent.clone(),
        insistent.clone(),
        insistent,
    ]));
    let caps = caps_with(llm);
    let (store, _root) = empty_store("gated");
    let ledger = MemoryLedger::new();

    let facts = HostFacts {
        workers: vec!["laptop".into(), "ci".into()],
        default_worker: Some("laptop".into()),
        ..HostFacts::unknown()
    };

    let err = decide(
        &Goal::new("do the thing"),
        "ep1",
        &store,
        &ledger,
        &facts,
        &caps,
        None,
    )
    .await
    .expect_err("a worker this host lacks must not reach the engine");

    assert!(
        err.to_string().contains("desktop"),
        "the error names it: {err}"
    );
    assert!(
        err.to_string().contains("laptop"),
        "and offers the alternatives: {err}"
    );
}

#[tokio::test]
async fn the_authoring_prompt_carries_what_the_host_permits() {
    // The facts below say agent work must name a worker, so the reply's ask
    // step names one — the same gate this test exists to see rendered.
    let llm = std::sync::Arc::new(Scripted::new(vec![
        select_declines(),
        json!({
            "why": "fine",
            "inputs": {},
            "steps": [{ "id": "work", "ask": "Do it directly.", "worker": "laptop" }],
        }),
    ]));
    let caps = caps_with(llm.clone());
    let (store, _root) = empty_store("facts-rendered");
    let ledger = MemoryLedger::new();

    let facts = HostFacts {
        workers: vec!["laptop".into()],
        default_worker: None,
        allow_code: Some(false),
        notes: vec!["Only manual triggers fire here.".into()],
        ..HostFacts::unknown()
    };

    decide(
        &Goal::new("anything"),
        "ep1",
        &store,
        &ledger,
        &facts,
        &caps,
        None,
    )
    .await
    .expect("decide");

    let prompt = &authoring_prompt(&llm);
    assert!(prompt.contains("What this host permits"), "{prompt}");
    assert!(prompt.contains("every agent node must name config.agent_ref"));
    assert!(prompt.contains("Only manual triggers fire here."));
}

// ---------------------------------------------------------------------------
// Promotion: a repaired family is one row, and score decides which.
// ---------------------------------------------------------------------------

/// A parent and one variant, both stored and linked, with scores applied.
async fn repaired_family(
    tag: &str,
    parent: (u32, u32),
    variant: (u32, u32),
) -> (FileWorkflowStore, MemoryLedger, std::path::PathBuf) {
    let (store, root) = empty_store(tag);
    store
        .save(&stored("weekly", "writes the weekly report", None))
        .expect("save");
    store
        .save(&stored(
            "weekly-fix-1",
            "writes the weekly report, with the binding corrected",
            None,
        ))
        .expect("save");

    let ledger = MemoryLedger::new();
    ledger
        .link_variant("weekly", "weekly-fix-1")
        .await
        .expect("link");
    for (id, (applied, helped)) in [("weekly", parent), ("weekly-fix-1", variant)] {
        for n in 0..applied {
            ledger.score_workflow(id, n < helped).await.expect("score");
        }
    }
    (store, ledger, root)
}

/// What the selector was actually shown.
async fn offered(store: &FileWorkflowStore, ledger: &MemoryLedger) -> String {
    let llm = std::sync::Arc::new(Scripted::new(vec![
        json!({"workflow_id": "none"}),
        authored_reply("fallback", None),
    ]));
    let caps = caps_with(llm.clone());
    let _ = decide(
        &Goal::new("write the weekly report"),
        "ep-promo",
        store,
        ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await;
    llm.prompts().first().cloned().unwrap_or_default()
}

#[tokio::test]
async fn a_repaired_family_is_offered_as_one_row_not_two() {
    // Two near-identical graphs whose descriptions differ by a clause is not a
    // choice, it is noise.
    let (store, ledger, _root) = repaired_family("promo-1", (40, 40), (0, 0)).await;
    let shown = offered(&store, &ledger).await;
    let rows = shown.matches("weekly").count();
    assert!(rows > 0, "the family must be offered at all: {shown}");
    assert!(
        !shown.contains("weekly-fix-1"),
        "an unproven variant must not appear beside its proven parent: {shown}"
    );
}

#[tokio::test]
async fn a_fresh_variant_does_not_displace_a_proven_parent() {
    let (store, ledger, _root) = repaired_family("promo-2", (40, 40), (0, 0)).await;
    let shown = offered(&store, &ledger).await;
    assert!(shown.contains("weekly"), "{shown}");
    assert!(!shown.contains("weekly-fix-1"), "{shown}");
}

#[tokio::test]
async fn a_variant_that_has_proven_better_is_the_one_offered() {
    // Promotion on score, not on having been written.
    let (store, ledger, _root) = repaired_family("promo-3", (10, 5), (4, 4)).await;
    let shown = offered(&store, &ledger).await;
    assert!(
        shown.contains("weekly-fix-1"),
        "the better member must take the position: {shown}"
    );
}

#[tokio::test]
async fn a_family_whose_champion_was_already_tried_still_offers_its_variant() {
    // The case that matters most and is easiest to get wrong: this episode just
    // failed with the parent, so the parent is excluded — and the variant
    // exists *because* the parent fell short. Dropping the whole family would
    // hide the one graph written for this exact situation.
    let (store, ledger, _root) = repaired_family("promo-4", (40, 40), (0, 0)).await;
    ledger
        .append(&tinyflows_adaptive::ledger::LedgerRow {
            id: String::new(),
            episode: "ep-promo".into(),
            attempt: 1,
            approach_sig: "selected:weekly".into(),
            approach_desc: "the champion".into(),
            workflow_id: Some("weekly".into()),
            outcome: "fell short".into(),
            cause: String::new(),
            cost_usd: 0.0,
            at: "2026-01-01T00:00:00Z".into(),
            satisfied: false,
            advanced: false,
        })
        .await
        .expect("append");

    let shown = offered(&store, &ledger).await;
    assert!(
        shown.contains("weekly-fix-1"),
        "the variant must survive its champion being excluded: {shown}"
    );
}

// ---------------------------------------------------------------------------
// The retry edge: attempt four must not be attempt two in different words.
// ---------------------------------------------------------------------------

async fn with_history(tag: &str) -> (FileWorkflowStore, MemoryLedger, std::path::PathBuf) {
    let (store, root) = empty_store(tag);
    let ledger = MemoryLedger::new();
    for (attempt, sig, desc, cause) in [
        (
            1u32,
            "authored:aaa",
            "fetched the log and summarised it",
            "no numbers in it",
        ),
        (
            2,
            "authored:bbb",
            "asked an agent to write it from memory",
            "it invented the figures",
        ),
    ] {
        ledger
            .append(&tinyflows_adaptive::ledger::LedgerRow {
                id: String::new(),
                episode: "ep-retry".into(),
                attempt,
                approach_sig: sig.into(),
                approach_desc: desc.into(),
                workflow_id: None,
                outcome: "fell short".into(),
                cause: cause.into(),
                cost_usd: 0.0,
                at: "2026-01-01T00:00:00Z".into(),
                satisfied: false,
                advanced: false,
            })
            .await
            .expect("append");
    }
    (store, ledger, root)
}

#[tokio::test]
async fn the_author_is_shown_what_this_episode_already_tried() {
    // Without this the author writes attempt two's graph again, confidently,
    // because nothing told it otherwise. The exclusion list only guards
    // *selection*; authoring has no structural guard at all.
    let (store, ledger, _root) = with_history("retry-1").await;
    let llm = std::sync::Arc::new(Scripted::new(vec![
        select_declines(),
        authored_reply("third-idea", None),
    ]));
    let caps = caps_with(llm.clone());

    decide(
        &Goal::new("write the weekly report"),
        "ep-retry",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect("decide");

    let prompt = &authoring_prompt(&llm);
    assert!(prompt.contains("Already tried this episode"), "{prompt}");
    assert!(
        prompt.contains("asked an agent to write it from memory"),
        "{prompt}"
    );
    assert!(prompt.contains("it invented the figures"), "{prompt}");
    assert!(prompt.contains("DIFFERENT plan"), "{prompt}");
}

#[tokio::test]
async fn the_selector_is_shown_the_same_history_in_the_same_words() {
    let (store, ledger, _root) = with_history("retry-2").await;
    store
        .save(&stored("weekly", "writes the weekly report", None))
        .expect("save");

    let llm = std::sync::Arc::new(Scripted::new(vec![json!({
        "workflow_id": "weekly",
        "why": "it does this",
        "inputs": {},
    })]));
    let caps = caps_with(llm.clone());

    decide(
        &Goal::new("write the weekly report"),
        "ep-retry",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect("decide");

    let prompt = &llm.prompts()[0];
    assert!(prompt.contains("Already tried this episode"), "{prompt}");
    assert!(prompt.contains("no numbers in it"), "{prompt}");
}

#[tokio::test]
async fn lessons_from_other_episodes_reach_the_planner() {
    // consolidate() was writing these and nothing was reading them — a
    // knowledge store that costs money and returns nothing.
    let (store, root) = empty_store("retry-3");
    let _ = root;
    let ledger = MemoryLedger::new();
    ledger
        .promote(
            &tinyflows_adaptive::ledger::Lesson {
                id: String::new(),
                kind: tinyflows_adaptive::ledger::LessonKind::Constraint,
                trigger: "a report that must cite figures".into(),
                mechanism: "the model has no access to the numbers".into(),
                claim: "read them from the source rather than asking an agent".into(),
                applied: 0,
                helped: 0,
                scope_key: None,
            },
            &[],
        )
        .await
        .expect("promote");

    let llm = std::sync::Arc::new(Scripted::new(vec![
        select_declines(),
        authored_reply("informed", None),
    ]));
    let caps = caps_with(llm.clone());

    decide(
        &Goal::new("write the weekly report"),
        "ep-fresh",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect("decide");

    let prompt = &authoring_prompt(&llm);
    assert!(prompt.contains("Learned from earlier episodes"), "{prompt}");
    assert!(prompt.contains("read them from the source"), "{prompt}");
}

#[tokio::test]
async fn a_first_attempt_is_told_nothing_it_would_have_to_ignore() {
    // An empty history section is noise a model has to read past, and an
    // empty "already tried" heading reads as a claim that something was.
    let (store, _root) = empty_store("retry-4");
    let ledger = MemoryLedger::new();
    let llm = std::sync::Arc::new(Scripted::new(vec![
        select_declines(),
        authored_reply("first", None),
    ]));
    let caps = caps_with(llm.clone());

    decide(
        &Goal::new("write the weekly report"),
        "ep-first",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect("decide");

    // Every prompt, not just one: an empty heading is noise whichever planner
    // reads it, and the triage call sees the same rendered past the author does.
    for prompt in llm.prompts() {
        assert!(!prompt.contains("Already tried"), "{prompt}");
        assert!(!prompt.contains("Learned from earlier"), "{prompt}");
    }
}

#[tokio::test]
async fn two_authored_attempts_leave_two_distinct_signatures() {
    // The fingerprint end to end: a differently-shaped graph must not fold into
    // the same exclusion-list entry as the one before it.
    let (store, _root) = empty_store("retry-5");
    let ledger = MemoryLedger::new();

    let mut signatures = Vec::new();
    for (n, name) in [(0, "shape-one"), (1, "shape-two")] {
        let llm = std::sync::Arc::new(Scripted::new(vec![
            select_declines(),
            authored_reply(name, if n == 1 { Some("repo") } else { None }),
        ]));
        let attempt = decide(
            &Goal::new("write the weekly report"),
            "ep-sigs",
            &store,
            &ledger,
            &HostFacts::unknown(),
            &caps_with(llm),
            None,
        )
        .await
        .expect("decide");
        signatures.push(attempt.approach.signature());
    }

    assert_ne!(signatures[0], signatures[1], "{signatures:?}");
    assert!(signatures[0].starts_with("authored:"), "{signatures:?}");
}
