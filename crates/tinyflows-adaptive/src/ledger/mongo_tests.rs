use super::*;
use crate::ledger::conformance;

/// Runs the same suite the sqlite backend passes, against a real server.
///
/// Ignored by default: it needs one. Point `ADAPTIVE_MONGO_URI` at a
/// throwaway database and run with `--ignored`. Skipping silently when the
/// variable is absent would let this rot unnoticed, so the case is
/// `#[ignore]` and visible in the run summary instead.
#[tokio::test]
#[ignore = "needs a MongoDB server; set ADAPTIVE_MONGO_URI"]
async fn passes_the_conformance_suite() {
    let uri = std::env::var("ADAPTIVE_MONGO_URI").expect("ADAPTIVE_MONGO_URI");
    let name = format!("adaptive_conformance_{}", std::process::id());
    let store = MongoLedger::connect(&uri, &name).await.expect("connect");
    conformance::run_all(&store).await;
    conformance::run_tenants(
        &store,
        &store.for_tenant("user-a"),
        &store.for_tenant("user-b"),
    )
    .await;
    store.db.drop().await.expect("drop the throwaway database");
}

#[tokio::test]
#[ignore = "needs a MongoDB server; set ADAPTIVE_MONGO_URI"]
async fn a_legacy_global_episode_without_a_scope_key_remains_readable_and_updatable() {
    let uri = std::env::var("ADAPTIVE_MONGO_URI").expect("ADAPTIVE_MONGO_URI");
    let name = format!("adaptive_legacy_episode_{}", std::process::id());
    let store = MongoLedger::connect(&uri, &name).await.expect("connect");
    let goal = crate::contracts::Goal::new("preserve the legacy episode");
    store
        .episodes_c()
        .insert_one(doc! {
            "_id": "legacy-global",
            "goal": serde_json::to_string(&goal).expect("serialize goal"),
            "status": serde_json::to_string(&EpisodeStatus::Running).expect("serialize status"),
            "attempt": 1_i64,
            "stalled": 0_i64,
            "started_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:05Z",
        })
        .await
        .expect("insert legacy episode");

    let mut episode = store
        .episode("legacy-global")
        .await
        .expect("read legacy episode")
        .expect("legacy episode exists");
    assert_eq!(episode.scope_key, None);
    assert_eq!(
        store
            .episodes(false, crate::ledger::Page::first(10))
            .await
            .expect("list episodes")
            .len(),
        1
    );

    episode.attempt = 2;
    store.save_episode(&episode).await.expect("update episode");
    assert_eq!(
        store
            .episode("legacy-global")
            .await
            .expect("read updated episode")
            .expect("updated episode exists")
            .attempt,
        2
    );
    assert_eq!(
        store
            .episodes_c()
            .count_documents(doc! { "$or": [
                { "id": "legacy-global" },
                { "_id": "legacy-global" },
            ] })
            .await
            .expect("count matching episodes"),
        1
    );

    store.db.drop().await.expect("drop the throwaway database");
}
