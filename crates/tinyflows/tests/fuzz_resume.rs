#![cfg(feature = "mock")]
//! Property tests for the checkpoint / resume path.
//!
//! # The property that matters
//!
//! Pausing a run must not change what the run computes. Concretely: a run that
//! is pre-approved and completes in one go, and a run that suspends at its gate
//! and is then resumed, must reach the **same final state**. Everything a
//! resume can get wrong — losing a branch's work, re-running a branch that had
//! already finished, replaying a fold twice — shows up as those two states
//! disagreeing.
//!
//! That equivalence is asserted here against generated graphs rather than one
//! hand-written flow, because the interesting resumes are the ones where the
//! gate sits *beside* concurrent work, and which branches are in flight at the
//! moment of suspension is precisely what a generator varies and a human does
//! not.
//!
//! # What "the same" has to mean here, and why
//!
//! The two paths cannot be compared byte-for-byte, and the reason is worth
//! stating so nobody later "fixes" this by weakening it further. Approval has
//! to reach the run through *some* channel, and the two channels are not
//! interchangeable: pre-approval rides in on the trigger payload, which every
//! node's items then carry, while a resume delivers approval out of band. So
//! the pre-approved run's items legitimately contain an `approvals` key the
//! resumed run's do not, and its slots carry different `_activation_step`
//! stamps because suspending genuinely costs a super-step.
//!
//! Comparison is therefore over what the *workflow* computed rather than how
//! the engine got there: which nodes ran, and how many items each produced.
//! That still catches the failures that matter — a branch whose work is lost
//! across the pause, a branch re-run so its output is duplicated, a node
//! skipped entirely on the resumed path.
//!
//! It does **not** catch a side-effecting node re-running idempotently, since
//! these graphs are built from pure passthroughs where a re-run looks identical
//! to not re-running. Detecting that needs an invocation counter rather than a
//! state diff, and belongs with the work that fixes it.
//!
//! Gated behind the `mock` feature alongside the rest of the e2e suite.

mod support;

use std::sync::Arc;
use std::time::Duration;

use proptest::prelude::*;
use serde_json::{Value, json};

use support::graphgen::{Shape, arb_gated_shape, graph_of};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::{
    InMemoryCheckpointer, resume_with_checkpointer, run, run_with_checkpointer,
};

/// How long any single run may take before it is called a hang.
const GUARD: Duration = Duration::from_secs(20);

/// Builds a private current-thread runtime for one property case.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime")
}

/// Reduces a final run state to "which nodes ran, and how many items each
/// emitted", sorted for a stable comparison.
///
/// This is the comparable core of a run: it survives the two approval channels
/// differing (see the module docs) while still changing the moment a node is
/// skipped, duplicated, or loses its output across a pause.
fn work_done(state: &Value) -> Vec<(String, usize)> {
    let mut work: Vec<(String, usize)> = state["nodes"]
        .as_object()
        .map(|slots| {
            slots
                .iter()
                .map(|(id, slot)| {
                    let count = slot["items"].as_array().map_or(0, Vec::len);
                    (id.clone(), count)
                })
                .collect()
        })
        .unwrap_or_default();
    work.sort();
    work
}

/// Runs `shape` two ways and returns `(pre_approved_state, resumed_state)`.
///
/// Returns `None` when the graph is refused by the validator, which is not what
/// these properties are about.
fn run_both_ways(shape: &Shape) -> Option<(Value, Value)> {
    let graph = graph_of(shape);
    let compiled = compile(&graph).ok()?;
    let gates = shape.gate_ids();
    assert!(
        !gates.is_empty(),
        "a gated shape must contain at least one gate, or this test proves nothing"
    );

    runtime().block_on(async {
        let caps = mock_capabilities();

        // Path A: every gate approved up front, so the run never suspends.
        let straight = tokio::time::timeout(
            GUARD,
            run(&compiled, json!({ "approvals": gates.clone() }), &caps),
        )
        .await
        .expect("pre-approved run hung")
        .expect("pre-approved run should complete");

        // Path B: no approvals, so the run suspends at its gate; then resume,
        // approving every gate, until it settles. A graph may hold more than
        // one gate, and a resume clears only the gates it names, so this loops
        // rather than assuming a single round trip.
        let checkpointer = Arc::new(InMemoryCheckpointer::default());
        let thread_id = "fuzz-resume";
        let mut outcome = tokio::time::timeout(
            GUARD,
            run_with_checkpointer(&compiled, json!({}), &caps, checkpointer.clone(), thread_id),
        )
        .await
        .expect("suspending run hung")
        .expect("suspending run should pause rather than fail");

        let mut rounds = 0;
        while !outcome.pending_approvals.is_empty() {
            rounds += 1;
            assert!(
                rounds <= gates.len() + 2,
                "resume did not converge after {rounds} rounds; still pending: {:?}",
                outcome.pending_approvals
            );
            let pending = outcome.pending_approvals.clone();
            outcome = tokio::time::timeout(
                GUARD,
                resume_with_checkpointer(
                    &compiled,
                    &caps,
                    checkpointer.clone(),
                    thread_id,
                    pending,
                ),
            )
            .await
            .expect("resume hung")
            .expect("resume should succeed");
        }

        Some((straight.output, outcome.output))
    })
}

/// Guards the properties below against passing vacuously.
///
/// Each of them returns early when a generated graph is refused by the
/// validator, so a generator that drifted into producing only invalid gated
/// graphs would leave them green while never once suspending a run. This pins
/// the yield so that drift fails here instead.
#[test]
fn gated_graphs_are_mostly_runnable_and_actually_suspend() {
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::TestRunner;

    const SAMPLES: usize = 60;
    const FLOOR: usize = 36; // 60%

    let mut runner = TestRunner::deterministic();
    let mut suspended = 0;
    for _ in 0..SAMPLES {
        let shape = arb_gated_shape(2)
            .new_tree(&mut runner)
            .expect("generate a shape")
            .current();
        let graph = graph_of(&shape);
        let Ok(compiled) = compile(&graph) else {
            continue;
        };
        let outcome = runtime().block_on(async {
            let caps = mock_capabilities();
            tokio::time::timeout(GUARD, run(&compiled, json!({}), &caps))
                .await
                .expect("run hung")
                .expect("an unapproved run should pause, not fail")
        });
        if !outcome.pending_approvals.is_empty() {
            suspended += 1;
        }
    }
    assert!(
        suspended >= FLOOR,
        "only {suspended}/{SAMPLES} generated gated graphs actually suspended, below the {FLOOR} \
         floor — the resume properties are running on very little"
    );
}

/// The shape `resuming_is_deterministic` shrank to when it caught a real race.
///
/// A gate sitting beside concurrent work, with three spawned tasks either side
/// of a branch — the arrangement in which the number of polls a gate needs was
/// decided by OS scheduling rather than by the graph.
fn regression_shape() -> Shape {
    Shape::Gate(Box::new(Shape::Branch(
        Box::new(Shape::Spawned {
            tasks: 3,
            release: "all",
            n: 3,
        }),
        Box::new(Shape::Fanout(vec![
            Shape::Linear(2),
            Shape::Linear(0),
            Shape::Spawned {
                tasks: 3,
                release: "all",
                n: 3,
            },
        ])),
    )))
}

/// **Regression.** A gate needs the same number of polls every time.
///
/// `resuming_is_deterministic` found this shape producing runs that agreed on
/// every collected item yet disagreed on the gate's `polls` count and on every
/// downstream `_activation_step`. The cause was not the gate: a `Reenter`
/// backoff whose timer had already fired completed without ever returning
/// `Pending`, so on a single-threaded runtime the spawned tasks the gate was
/// waiting on got no turn to run, and the gate spent an extra poll observing a
/// world that had not moved. Whether that happened depended on whether the
/// thread was descheduled for longer than the poll interval — which is why the
/// property test only ever failed on a loaded CI machine.
///
/// Run as a fixed loop rather than a property, because the shape is already
/// known and the variable being exercised is repetition, not generation. It
/// reproduced within a few hundred iterations under load before the fix.
#[test]
fn a_gate_takes_the_same_number_of_polls_every_run() {
    const RUNS: usize = 150;

    let shape = regression_shape();
    let mut baseline: Option<Value> = None;
    for run_index in 0..RUNS {
        let (_, resumed) = run_both_ways(&shape).expect("the regression shape should compile");
        match &baseline {
            None => baseline = Some(resumed),
            Some(first) => assert_eq!(
                first, &resumed,
                "run {run_index} of the same shape differed from the first; a gate's poll count \
                 or an activation step is being decided by scheduling rather than by the graph"
            ),
        }
    }
}

proptest! {
    // Deliberately fewer cases than the other fuzz files: each case runs the
    // same graph at least twice, once through the checkpointer.
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// **Resume equivalence.** Suspending and resuming a run does the same work
    /// as never suspending at all — same nodes, same item counts.
    ///
    /// See the module docs for why this is a work comparison rather than a
    /// byte comparison.
    #[test]
    fn a_resumed_run_does_the_same_work_as_an_uninterrupted_one(shape in arb_gated_shape(2)) {
        let Some((straight, resumed)) = run_both_ways(&shape) else {
            return Ok(());
        };
        prop_assert_eq!(
            work_done(&straight), work_done(&resumed),
            "pausing changed which nodes ran or how much they emitted\nshape: {:?}",
            shape
        );
    }

    /// Resuming is itself deterministic: two identical suspend-and-resume
    /// cycles produce byte-identical state.
    ///
    /// Unlike the property above this *can* compare exactly, because both runs
    /// take the same path through the same channel — which makes it the
    /// stricter of the two whenever it applies.
    #[test]
    fn resuming_is_deterministic(shape in arb_gated_shape(2)) {
        let Some((_, first)) = run_both_ways(&shape) else {
            return Ok(());
        };
        let Some((_, second)) = run_both_ways(&shape) else {
            return Ok(());
        };
        prop_assert_eq!(
            &first, &second,
            "two identical resume cycles disagreed\nshape: {:?}",
            shape
        );
    }

    /// A run holding an unapproved gate must actually pause — reporting the
    /// gate as pending rather than quietly running through it.
    ///
    /// Without this, the equivalence property above could be satisfied by a
    /// gate that never gates: both paths would agree because neither ever
    /// suspended, and the whole file would prove nothing.
    #[test]
    fn an_unapproved_gate_actually_suspends_the_run(shape in arb_gated_shape(1)) {
        let graph = graph_of(&shape);
        let Ok(compiled) = compile(&graph) else {
            return Ok(());
        };
        let outcome = runtime().block_on(async {
            let caps = mock_capabilities();
            tokio::time::timeout(GUARD, run(&compiled, json!({}), &caps))
                .await
                .expect("run hung")
                .expect("an unapproved run should pause, not fail")
        });
        prop_assert!(
            !outcome.pending_approvals.is_empty(),
            "a graph with an unapproved gate completed without pausing"
        );
    }
}
