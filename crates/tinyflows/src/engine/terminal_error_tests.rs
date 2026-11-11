//! Unit tests for choosing a run's reported error among several failures.

use super::*;

fn failure(name: &str) -> EngineError {
    EngineError::Capability(name.to_string())
}

fn reported(slot: &Option<TerminalError>) -> Option<String> {
    slot.as_ref().map(|held| held.error.to_string())
}

fn expected(name: &str) -> Option<String> {
    Some(failure(name).to_string())
}

#[test]
fn the_lowest_branch_wins_whichever_arrives_first() {
    let mut lowest_first = None;
    offer(&mut lowest_first, 2, 0, failure("branch 0"));
    offer(&mut lowest_first, 2, 1, failure("branch 1"));

    let mut lowest_last = None;
    offer(&mut lowest_last, 2, 1, failure("branch 1"));
    offer(&mut lowest_last, 2, 0, failure("branch 0"));

    assert_eq!(reported(&lowest_first), expected("branch 0"));
    assert_eq!(reported(&lowest_last), expected("branch 0"));
}

#[test]
fn an_earlier_superstep_is_not_replaced_by_a_lower_branch_later() {
    let mut slot = None;
    offer(&mut slot, 1, 3, failure("step 1"));
    offer(&mut slot, 2, 0, failure("step 2"));
    assert_eq!(reported(&slot), expected("step 1"));
}

#[test]
fn a_single_failure_is_reported_as_is() {
    let mut slot = None;
    offer(&mut slot, 4, 2, failure("only"));
    assert_eq!(reported(&slot), expected("only"));
}
