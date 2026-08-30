//! Tests for the controller's registration and release rules.
//!
//! The parking behaviour itself is covered end-to-end in `session_tests`, where
//! a real run is driven from another task; what is under test here is what the
//! controller refuses and what it reports.

use super::*;
use std::time::Duration;

#[test]
fn a_durable_after_breakpoint_is_refused_at_registration() {
    // The reason this is a hard error rather than a footgun: a durable pause is
    // a real interrupt, and resuming re-runs the interrupted node from the top.
    // Breaking *after* one would run its side effects twice.
    let (controller, _pauses) = DebugController::new();
    let spec = BreakpointSpec::after("send").durable();

    let err = controller
        .set_breakpoint(spec)
        .expect_err("a durable after-breakpoint must be refused");
    let message = err.to_string();
    assert!(message.contains("re-runs"), "got {message}");
}

#[test]
fn a_durable_before_breakpoint_is_allowed() {
    // Nothing has run yet, so the re-run costs nothing.
    let (controller, _pauses) = DebugController::new();
    controller
        .set_breakpoint(BreakpointSpec::before("send").durable())
        .expect("a durable before-breakpoint is fine");
}

#[test]
fn a_breakpoint_that_breaks_at_neither_phase_is_refused() {
    let (controller, _pauses) = DebugController::new();
    let mut spec = BreakpointSpec::before("send");
    spec.before = false;
    assert!(controller.set_breakpoint(spec).is_err());
}

#[test]
fn breakpoints_are_listed_with_their_hit_counts() {
    let (controller, _pauses) = DebugController::new();
    let id = controller
        .set_breakpoint(BreakpointSpec::before("a"))
        .expect("registers");

    let listed = controller.breakpoints();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].hits, 0);
    assert!(listed[0].enabled);
}

#[test]
fn clearing_reports_whether_it_removed_anything() {
    let (controller, _pauses) = DebugController::new();
    let id = controller
        .set_breakpoint(BreakpointSpec::before("a"))
        .expect("registers");

    assert!(controller.clear_breakpoint(id));
    assert!(
        !controller.clear_breakpoint(id),
        "clearing twice should report that there was nothing to clear"
    );
    assert!(controller.breakpoints().is_empty());
}

#[test]
fn releasing_an_unknown_pause_is_refused_rather_than_ignored() {
    // A stale command means the caller's view is out of date, and silently
    // dropping it would leave them believing the run moved on.
    let (controller, _pauses) = DebugController::new();
    let err = controller
        .release(999, DebugCommand::Continue)
        .expect_err("an unknown pause id must be refused");
    assert!(err.to_string().contains("999"), "got {err}");
}

#[test]
fn detaching_clears_breakpoints_and_marks_the_controller_inert() {
    let (controller, _pauses) = DebugController::new();
    controller
        .set_breakpoint(BreakpointSpec::before("a"))
        .expect("registers");

    controller.detach();

    assert!(controller.is_detached());
    assert!(controller.breakpoints().is_empty());
    assert!(controller.pauses().is_empty());
}

#[test]
fn the_pause_timeout_is_configurable_including_off() {
    let (controller, _pauses) = DebugController::new();
    controller.set_pause_timeout(Some(Duration::from_millis(10)));
    // Disabling it is the one setting that can hang a run, so it has to be
    // asked for rather than defaulted to.
    controller.set_pause_timeout(None);
}
