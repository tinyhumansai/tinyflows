//! Closing, end to end, against a scripted model and a real ledger.
//!
//! The unit tests cover the decision table and the parsing. These cover the
//! properties that only show up once a ledger is actually written to: that a
//! *failed* attempt still leaves a row and still moves the score, that the row
//! it leaves is the one the next attempt's exclusion list reads, and that the
//! three mechanical verdicts never reach the model at all.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::caps::{Capabilities, LlmProvider};
use tinyflows::diagnostics::{Diagnosis, NeverRan};
use tinyflows::engine::RunOutcome;
use tinyflows::error::Result as EngineResult;
use tinyflows::model::WorkflowGraph;
use tinyflows_adaptive::closing::{Next, close, consolidate};
use tinyflows_adaptive::contracts::{Approach, Blocker, Budget, Goal};
use tinyflows_adaptive::execute::Ran;
use tinyflows_adaptive::ledger::{Ledger, LessonKind, memory::MemoryLedger};

/// A provider that answers from a script and counts what it was asked.
struct Scripted {
    replies: Mutex<Vec<Value>>,
    calls: Mutex<Vec<String>>,
}

impl Scripted {
    fn new(replies: Vec<Value>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            replies: Mutex::new(replies),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn call_count(&self) -> usize {
        self.calls.lock().expect("lock").len()
    }

    fn last_prompt(&self) -> String {
        self.calls
            .lock()
            .expect("lock")
            .last()
            .cloned()
            .unwrap_or_default()
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
        self.calls.lock().expect("lock").push(text);
        let mut replies = self.replies.lock().expect("lock");
        assert!(
            !replies.is_empty(),
            "the model was asked more times than the script has answers"
        );
        Ok(replies.remove(0))
    }
}

fn caps_with(llm: std::sync::Arc<Scripted>) -> Capabilities {
    Capabilities {
        llm,
        ..mock_capabilities()
    }
}

fn completed(output: Value) -> RunOutcome {
    RunOutcome {
        output,
        pending_approvals: Vec::new(),
        cancelled: false,
    }
}

/// A finished run, as `close` now takes it. The judge still reads only the
/// evidence; the cost and the transcript ride along because they are recorded.
fn ran(outcome: &RunOutcome, diagnosis: &Diagnosis, changed: &str) -> Ran {
    Ran {
        outcome: outcome.clone(),
        diagnosis: diagnosis.clone(),
        changed: changed.to_string(),
        failed: None,
        steps: Vec::new(),
        cost_usd: 0.0,
        resume: None,
    }
}

fn selected(id: &str) -> Approach {
    Approach::Selected {
        workflow_id: id.to_string(),
        why: "it matched".into(),
    }
}

#[tokio::test]
async fn a_failed_attempt_is_still_recorded_and_still_scored() {
    // The property the whole retry edge rests on. An attempt that fell short
    // and left no trace is one the next attempt repeats verbatim.
    let llm = Scripted::new(vec![json!({
        "satisfied": false,
        "blocker": "goal_not_met",
        "gap": "the report has no numbers in it",
        "advanced": true
    })]);
    let ledger = MemoryLedger::new();
    let diagnosis = Diagnosis::default();
    let outcome = completed(json!({"nodes": {"write": {"ok": true}}}));

    let closed = close(
        &Goal::new("write the weekly report"),
        "ep-1",
        1,
        &selected("weekly"),
        &WorkflowGraph::default(),
        &ran(&outcome, &diagnosis, "wrote report.md"),
        &Budget::default(),
        &ledger,
        &caps_with(llm),
        None,
        "2026-01-01T00:00:00Z",
    )
    .await
    .expect("closed");

    assert_eq!(closed.next, Next::Retry);
    assert_eq!(closed.stalled, 0, "it advanced, so nothing is stalling yet");

    let rows = ledger.rows("ep-1").await.expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, "the report has no numbers in it");
    assert_eq!(rows[0].workflow_id.as_deref(), Some("weekly"));

    // The exclusion list the next attempt reads.
    assert_eq!(ledger.tried("ep-1").await.expect("tried").len(), 1);

    let score = ledger.workflow_score("weekly").await.expect("score");
    assert_eq!(
        (score.applied, score.helped),
        (1, 0),
        "a run that failed still counts as a run"
    );
}

#[tokio::test]
async fn a_satisfied_attempt_moves_both_halves_of_the_score() {
    let llm = Scripted::new(vec![json!({"satisfied": true, "gap": ""})]);
    let ledger = MemoryLedger::new();
    let diagnosis = Diagnosis::default();
    let outcome = completed(json!({"nodes": {"write": {"ok": true}}}));

    let closed = close(
        &Goal::new("write the weekly report"),
        "ep-2",
        1,
        &selected("weekly"),
        &WorkflowGraph::default(),
        &ran(&outcome, &diagnosis, "wrote report.md"),
        &Budget::default(),
        &ledger,
        &caps_with(llm),
        None,
        "2026-01-01T00:00:00Z",
    )
    .await
    .expect("closed");

    assert_eq!(closed.next, Next::Done);
    assert_eq!(closed.verdict.blocker, Blocker::None);
    let score = ledger.workflow_score("weekly").await.expect("score");
    assert_eq!((score.applied, score.helped), (1, 1));
}

#[tokio::test]
async fn a_run_where_nothing_happened_never_reaches_the_model() {
    // Mechanical evidence first. The script is empty on purpose: if the judge
    // asks anything at all, `Scripted` panics and this test fails.
    let llm = Scripted::new(Vec::new());
    let caps = caps_with(llm.clone());
    let ledger = MemoryLedger::new();
    let diagnosis = Diagnosis {
        never_ran: vec![NeverRan {
            node_id: "write".into(),
            routed_by: Some("is_due".into()),
        }],
        ..Diagnosis::default()
    };
    let outcome = completed(json!({}));

    let closed = close(
        &Goal::new("write the weekly report"),
        "ep-3",
        1,
        &selected("weekly"),
        &WorkflowGraph::default(),
        &ran(&outcome, &diagnosis, ""),
        &Budget::default(),
        &ledger,
        &caps,
        None,
        "2026-01-01T00:00:00Z",
    )
    .await
    .expect("closed");

    assert_eq!(llm.call_count(), 0, "a fact does not need an opinion");
    assert_eq!(closed.verdict.blocker, Blocker::MissingEvidence);
    // Terminal: a retry with the same inputs produces the same nothing.
    assert!(
        matches!(closed.next, Next::StandDown(_)),
        "{:?}",
        closed.next
    );
    // And it is still on the record.
    assert_eq!(ledger.rows("ep-3").await.expect("rows").len(), 1);
}

#[tokio::test]
async fn a_parked_approval_is_not_a_failure() {
    let llm = Scripted::new(Vec::new());
    let caps = caps_with(llm.clone());
    let ledger = MemoryLedger::new();
    let diagnosis = Diagnosis::default();
    let outcome = RunOutcome {
        output: json!({"nodes": {"draft": {"ok": true}}}),
        pending_approvals: vec!["publish".into()],
        cancelled: false,
    };

    let closed = close(
        &Goal::new("publish the post"),
        "ep-4",
        1,
        &selected("blog"),
        &WorkflowGraph::default(),
        &ran(&outcome, &diagnosis, ""),
        &Budget::default(),
        &ledger,
        &caps,
        None,
        "2026-01-01T00:00:00Z",
    )
    .await
    .expect("closed");

    assert_eq!(llm.call_count(), 0);
    assert_eq!(closed.verdict.blocker, Blocker::NeedsInput);
    assert_eq!(
        closed.stalled, 0,
        "reaching an approval gate is progress, not a stall"
    );
}

#[tokio::test]
async fn two_flat_attempts_in_a_row_stand_down_on_the_stall_rule() {
    let llm = Scripted::new(vec![
        json!({"satisfied": false, "blocker": "goal_not_met", "gap": "same as before", "advanced": false}),
        json!({"satisfied": false, "blocker": "goal_not_met", "gap": "same as before", "advanced": false}),
    ]);
    let caps = caps_with(llm);
    let ledger = MemoryLedger::new();
    let diagnosis = Diagnosis::default();
    let outcome = completed(json!({"nodes": {"write": {}}}));
    let budget = Budget::default();

    let mut stalled = 0;
    let mut last = None;
    for attempt in 4..6 {
        let closed = close(
            &Goal::new("write the weekly report"),
            "ep-5",
            attempt,
            &Approach::Authored {
                why: format!("attempt {attempt}"),
                fingerprint: "0000000".into(),
            },
            &WorkflowGraph::default(),
            &ran(&outcome, &diagnosis, ""),
            &budget,
            &ledger,
            &caps,
            None,
            "2026-01-01T00:00:00Z",
        )
        .await
        .expect("closed");
        stalled = closed.stalled;
        last = Some(closed.next);
    }

    assert_eq!(stalled, 2);
    match last.expect("a second pass ran") {
        Next::StandDown(reason) => assert!(reason.contains("no progress"), "{reason}"),
        other => panic!("expected a stand-down after two flat attempts, got {other:?}"),
    }
    // Authoring attempts have no workflow to score, and scoring one anyway
    // would credit whichever workflow happened to run last.
    assert_eq!(
        ledger.rows("ep-5").await.expect("rows")[0].workflow_id,
        None
    );
}

#[tokio::test]
async fn consolidation_keeps_a_lesson_and_cites_the_rows_behind_it() {
    let ledger = MemoryLedger::new();
    for (attempt, sig) in [(1u32, "sig-a"), (2, "sig-b")] {
        ledger
            .append(&tinyflows_adaptive::ledger::LedgerRow {
                id: String::new(),
                episode: "ep-6".into(),
                attempt,
                approach_sig: sig.into(),
                approach_desc: "tried it".into(),
                workflow_id: None,
                outcome: "fell short".into(),
                cause: "the loop never terminated".into(),
                cost_usd: 0.0,
                at: "2026-01-01T00:00:00Z".into(),
                satisfied: false,
                advanced: false,
            })
            .await
            .expect("appended");
    }

    let llm = Scripted::new(vec![json!({
        "lessons": [{
            "kind": "constraint",
            "trigger": "a scan over ~1M items with a sub-100ms target",
            "mechanism": "the interpreter overhead dominates",
            "claim": "reach for a compiled step instead of tuning the loop",
            "evidence": [0, 1]
        }],
        "corroborate": []
    })]);
    let caps = caps_with(llm.clone());

    let kept = consolidate(
        &Goal::new("make the scan fast"),
        "ep-6",
        false,
        &ledger,
        &caps,
        None,
    )
    .await;

    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].kind, LessonKind::Constraint);
    assert!(!kept[0].id.is_empty(), "it was actually stored");

    // The rows the claim was drawn from, readable back.
    let cites = ledger.evidence(&kept[0].id).await.expect("evidence");
    assert_eq!(cites.len(), 2);

    // Both attempts were shown, numbered the way the prompt asks it to cite.
    let prompt = llm.last_prompt();
    assert!(prompt.contains("0. [sig-a]"), "{prompt}");
    assert!(prompt.contains("1. [sig-b]"), "{prompt}");
}

#[tokio::test]
async fn a_lesson_with_nothing_behind_it_is_not_kept() {
    // A claim with no rows cited is a guess, and a guess in the knowledge store
    // is worse than nothing: it will be retrieved and believed.
    let ledger = MemoryLedger::new();
    ledger
        .append(&tinyflows_adaptive::ledger::LedgerRow {
            id: String::new(),
            episode: "ep-7".into(),
            attempt: 1,
            approach_sig: "sig-a".into(),
            approach_desc: "tried it".into(),
            workflow_id: None,
            outcome: "fell short".into(),
            cause: String::new(),
            cost_usd: 0.0,
            at: "2026-01-01T00:00:00Z".into(),
            satisfied: false,
            advanced: false,
        })
        .await
        .expect("appended");

    let llm = Scripted::new(vec![json!({
        "lessons": [
            {"kind": "strategy", "trigger": "a class of task", "claim": "do the thing"},
            {"kind": "strategy", "claim": "no trigger, so nothing could ever match it",
             "evidence": [0]}
        ]
    })]);

    let kept = consolidate(
        &Goal::new("make the scan fast"),
        "ep-7",
        false,
        &ledger,
        &caps_with(llm),
        None,
    )
    .await;

    assert!(kept.is_empty(), "{kept:?}");
    assert!(ledger.lessons(None).await.expect("lessons").is_empty());
}

#[tokio::test]
async fn consolidation_failing_does_not_fail_the_episode() {
    // It runs after the outcome is settled. A provider hiccup keeps nothing and
    // leaves the real result standing — note the signature has no `Result`.
    let ledger = MemoryLedger::new();
    ledger
        .append(&tinyflows_adaptive::ledger::LedgerRow {
            id: String::new(),
            episode: "ep-8".into(),
            attempt: 1,
            approach_sig: "sig-a".into(),
            approach_desc: "tried it".into(),
            workflow_id: None,
            outcome: "fell short".into(),
            cause: String::new(),
            cost_usd: 0.0,
            at: "2026-01-01T00:00:00Z".into(),
            satisfied: false,
            advanced: false,
        })
        .await
        .expect("appended");

    // Not JSON the reader can use.
    let llm = Scripted::new(vec![json!("the model wandered off into prose")]);
    let kept = consolidate(
        &Goal::new("make the scan fast"),
        "ep-8",
        false,
        &ledger,
        &caps_with(llm),
        None,
    )
    .await;
    assert!(kept.is_empty());
}

#[tokio::test]
async fn an_episode_with_no_attempts_asks_nothing() {
    let llm = Scripted::new(Vec::new());
    let caps = caps_with(llm.clone());
    let ledger = MemoryLedger::new();
    let kept = consolidate(
        &Goal::new("anything"),
        "ep-none",
        true,
        &ledger,
        &caps,
        None,
    )
    .await;
    assert!(kept.is_empty());
    assert_eq!(llm.call_count(), 0);
}

include!("closing/closing_part_01_tests.rs");
