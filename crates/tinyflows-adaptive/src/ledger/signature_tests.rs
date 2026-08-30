use super::{LedgerRow, signatures};

fn row(attempt: u32, sig: &str) -> LedgerRow {
    LedgerRow {
        id: format!("r{attempt}"),
        episode: "ep".into(),
        attempt,
        approach_sig: sig.into(),
        approach_desc: String::new(),
        workflow_id: None,
        outcome: String::new(),
        cause: String::new(),
        cost_usd: 0.0,
        at: "2026-01-01T00:00:00Z".into(),
        satisfied: false,
        advanced: false,
    }
}

#[test]
fn an_approach_tried_twice_appears_once() {
    let got = signatures(&[
        row(1, "selected:weekly"),
        row(2, "authored:aaa"),
        row(3, "selected:weekly"),
    ]);
    assert_eq!(got, vec!["selected:weekly", "authored:aaa"]);
}

#[test]
fn first_seen_order_is_kept() {
    // It is rendered into a prompt, and a list that reshuffles between
    // attempts is one a planner cannot be reasoned about against.
    let got = signatures(&[row(1, "c"), row(2, "a"), row(3, "b")]);
    assert_eq!(got, vec!["c", "a", "b"]);
}

#[test]
fn no_rows_is_an_empty_list_rather_than_a_surprise() {
    assert!(signatures(&[]).is_empty());
}

#[tokio::test]
async fn the_trait_method_agrees_with_the_function_it_now_calls() {
    // `tried` is this over a fresh read. If the two ever disagree, one
    // caller's exclusion list is not the other's.
    use super::Ledger;
    let ledger = super::memory::MemoryLedger::new();
    for (attempt, sig) in [
        (1u32, "selected:weekly"),
        (2, "authored:aaa"),
        (3, "selected:weekly"),
    ] {
        ledger.append(&row(attempt, sig)).await.expect("append");
    }
    let rows = ledger.rows("ep").await.expect("rows");
    assert_eq!(ledger.tried("ep").await.expect("tried"), signatures(&rows));
}
