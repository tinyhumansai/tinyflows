/// Run every lineage case. Part of [`run_all`]'s contract for any backend that
/// stores variant links. Both shipped backends do.
///
/// # Panics
/// On any lineage failure.
pub async fn run_lineage(store: &dyn Ledger) {
    an_unlinked_workflow_is_a_family_of_one(store).await;
    lineage_reads_the_same_from_any_member(store).await;
    linking_the_same_pair_twice_is_a_no_op(store).await;
    a_variant_of_a_variant_stays_in_one_family(store).await;
    a_cycle_is_truncated_rather_than_hung(store).await;
}

async fn an_unlinked_workflow_is_a_family_of_one(store: &dyn Ledger) {
    let family = store.lineage("wf-lonely").await.expect("lineage");
    assert_eq!(family, vec!["wf-lonely".to_string()]);
}

async fn lineage_reads_the_same_from_any_member(store: &dyn Ledger) {
    store
        .link_variant("wf-a", "wf-a-fix-1")
        .await
        .expect("link");
    store
        .link_variant("wf-a", "wf-a-fix-2")
        .await
        .expect("link");

    let from_root = store.lineage("wf-a").await.expect("lineage");
    let from_leaf = store.lineage("wf-a-fix-2").await.expect("lineage");
    assert_eq!(
        from_root, from_leaf,
        "the champion must not depend on which member was asked"
    );
    assert_eq!(
        from_root[0], "wf-a",
        "root first — the fallback relies on it"
    );
    assert_eq!(from_root.len(), 3);
}

async fn linking_the_same_pair_twice_is_a_no_op(store: &dyn Ledger) {
    // A repair converging on an existing variant id will re-link. It must not
    // duplicate the family member.
    store
        .link_variant("wf-b", "wf-b-fix-1")
        .await
        .expect("link");
    store
        .link_variant("wf-b", "wf-b-fix-1")
        .await
        .expect("link");
    assert_eq!(store.lineage("wf-b").await.expect("lineage").len(), 2);
}

async fn a_variant_of_a_variant_stays_in_one_family(store: &dyn Ledger) {
    // `repair` takes whatever ran as the parent, and what ran may itself be a
    // variant. Two generations are still one family, or the grandchild would be
    // compared against nothing.
    store
        .link_variant("wf-c", "wf-c-fix-1")
        .await
        .expect("link");
    store
        .link_variant("wf-c-fix-1", "wf-c-fix-2")
        .await
        .expect("link");

    let family = store.lineage("wf-c-fix-2").await.expect("lineage");
    assert_eq!(family[0], "wf-c");
    assert_eq!(family.len(), 3, "{family:?}");
}

async fn a_cycle_is_truncated_rather_than_hung(store: &dyn Ledger) {
    // Nothing should write this, but the ledger is read on the hot path of
    // every attempt and a hang there stops the whole loop. Bounded walks mean
    // a corrupt link costs a truncated answer instead.
    store.link_variant("wf-y", "wf-x").await.expect("link");
    store.link_variant("wf-x", "wf-y").await.expect("link");
    let family = store.lineage("wf-x").await.expect("lineage");
    assert!(family.len() <= super::MAX_FAMILY, "{family:?}");
}
