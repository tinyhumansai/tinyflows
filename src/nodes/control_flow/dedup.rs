//! The `dedup` node: the **filter** half of a commit-on-success exactly-once
//! pattern.
//!
//! A `dedup` node drops an item whose per-item `config.key` (an `"=expr"`, e.g.
//! `"=item.id"`) has already been durably **committed** by a prior successful
//! run, and otherwise passes the item through while recording the key as
//! **tentative** — pending a decision this node never makes itself. Whether a
//! run's tentative keys get promoted to committed (success) or discarded
//! (failure) is entirely a host concern, driven off `StateStore` after the run
//! completes. This node is deliberately committer-agnostic: it only ever reads
//! `committed` and writes `tentative`.
//!
//! # Why this lives in `control_flow`, not `integration`
//!
//! Every other node in this module is pure — no host capability, just routing
//! and reshaping of in-flight data. `dedup` is the one exception: it reads and
//! writes [`StateStore`](crate::caps::StateStore) durable state. It sits here
//! anyway because *semantically* it is a filter over the data flow (like
//! `condition`/`split_out`), not a delivery surface onto an external system
//! (like `tool_call`/`memory`) — `StateStore` is closer to compiler-managed
//! run bookkeeping than to a capability an author is "calling out" to.
//!
//! # The `StateStore` contract (read this before writing a host subscriber)
//!
//! [`Capabilities::state`](crate::caps::Capabilities::state) is documented as
//! exact-key and durable; hosts additionally namespace it per-flow (so two
//! different saved flows never see each other's keys) *before* handing the
//! `StateStore` to the engine — this node does not know about, and does not
//! need to know about, that outer namespace. What this node DOES own is the
//! discriminator **within** a flow's namespace: two `dedup` nodes in the same
//! flow graph must never collide, so every key this node touches is prefixed
//! with its own [`Node::id`](crate::model::Node::id).
//!
//! For a `dedup` node with id `"<node_id>"`, exactly two `StateStore` keys
//! exist, built by [`committed_key`] and [`tentative_key`]:
//!
//! | Key | Written by | Read by | Meaning |
//! |---|---|---|---|
//! | `dedup:<node_id>:committed` | the **host**, after a run succeeds | this node (every run) | keys proven seen — permanent |
//! | `dedup:<node_id>:tentative` | this node (every run with an unseen key) | the **host**, after a run finishes | keys passed *this* run, awaiting a commit/release decision |
//!
//! Each key's stored [`Value`] is a JSON array of strings (order not
//! meaningful; treat as a set) — chosen over per-member keys because
//! `StateStore` exposes no "list keys with prefix" or "delete by prefix"
//! operation, so one array under one key is the only shape a host can
//! atomically read-modify-write or clear in a single `load`/`store` round
//! trip.
//!
//! **The host-side contract this node depends on** (a `DedupCommitSubscriber`,
//! PR 2's job, not this crate's):
//! - **On run success**: for every `dedup` node that ran, union its
//!   `tentative` set into its `committed` set, then clear `tentative` (store
//!   `[]`, or delete the key if the store supports it) so a later run's load
//!   starts from empty. `committed` is APPEND-ONLY from this node's point of
//!   view — this node never removes a committed key.
//! - **On run failure**: clear (release) `tentative` for every `dedup` node
//!   that ran, WITHOUT touching `committed`. A released key is exactly as
//!   unseen as it was before the run — the next run may pass it through again.
//! - **This node commits nothing itself.** Directly inspecting `committed`
//!   after a run proves that: see
//!   `committed_set_is_never_mutated_by_the_node_itself` in this module's
//!   tests.
//!
//! # Runtime behavior (per item)
//!
//! 1. Resolve `config.key` against the item (same `"=expr"` resolver as
//!    `condition`/`memory`, via [`crate::nodes::resolve_config_traced_for_item`]).
//! 2. A key that resolves to `null`, is absent, or is an empty string
//!    **fails open**: the item passes through and is NOT recorded anywhere.
//!    An item is never silently dropped just because its key could not be
//!    computed — see [`resolve_key`].
//! 3. Otherwise, if the key is already in `committed` **or** was already seen
//!    earlier in this same batch, the item is dropped.
//! 4. Otherwise, the item passes through and the key is added to this run's
//!    tentative additions (written to `StateStore` once, after the batch).
//!
//! A duplicate key appearing twice in one run's input passes exactly once —
//! the first occurrence wins, later ones are dropped by the same rule as an
//! already-committed key (see `same_key_twice_in_one_run_passes_once_only`).
//!
//! # Ports
//!
//! Single `main` output, carrying only the items that passed (unseen). Unlike
//! `condition`'s `true`/`false`, [`NodeOutput`] carries one item list and one
//! optional port per execution — there is no way to emit the dropped items on
//! a second `skipped` port in the same call, so this node does not attempt
//! one; deduped-out items are simply absent from `main`. A host that wants
//! observability into what was dropped reads the debug log
//! (`[dedup] key already seen — dropping item`) or diffs input against output.
//!
//! # Key canonicalization
//!
//! `config.key` should resolve to a stable scalar id (issue number, message id,
//! url). A non-string scalar (`Number`/`Bool`) is canonicalized via its JSON
//! text so it still dedupes deterministically. An array/object also resolves to
//! its (deterministic) JSON text, but using a whole structure as a dedup key is
//! almost always an authoring mistake — prefer a scalar id. `resolve_key` does
//! not reject structured keys (they are deterministic, so a hard error would be
//! more surprising than useful), but authors should treat a structured key as a
//! smell.
//!
//! # Concurrency and the scope of the exactly-once guarantee
//!
//! `StateStore` exposes only `load`/`store` — there is **no** compare-and-swap
//! or atomic read-modify-write. The `tentative` write here (and the host's
//! `committed` union after the run) is therefore a `load` → merge → `store`
//! that is **not** safe against *two runs of the same flow overlapping in
//! time*: two concurrent `load`s can each miss the other's not-yet-stored keys,
//! and the later `store` wins (lost update).
//!
//! The exactly-once guarantee holds under the normal assumption that a given
//! saved flow has **at most one active run at a time** (the usual case: a
//! scheduled or singly-triggered flow). When two runs of the *same* flow truly
//! overlap, the failure is bounded and in the **safe direction**: a lost
//! tentative/committed key can only cause an item to be processed **again** on
//! a later run (an occasional duplicate — *under*-dedup), never to be wrongly
//! dropped (*over*-dedup). No item is ever silently lost.
//!
//! Closing this window fully requires an atomic `StateStore` primitive
//! (compare-and-swap or a per-key serialized update), which is a deliberate
//! **deferred** engine change — not added here because the current failure mode
//! is rare and benign. A host can already narrow the durable half of the race
//! by serializing its post-run `committed` update per flow id (the OpenHuman
//! `DedupCommitSubscriber` does exactly this).

use std::collections::HashSet;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::Result;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Stable `tracing` grep prefix for every log line this node emits.
const LOG_PREFIX: &str = "[dedup]";

/// Commit-on-success exactly-once filter — the FILTER half. See the module
/// docs for the full `StateStore` contract.
#[derive(Debug, Default, Clone)]
pub struct DedupNode;

/// The `StateStore` key holding `node_id`'s durable, host-committed key set.
///
/// Read by this node on every run; written only by the host (never by this
/// node) once a run succeeds. See the module docs for the full contract.
#[must_use]
pub fn committed_key(node_id: &str) -> String {
    format!("dedup:{node_id}:committed")
}

/// The `StateStore` key holding `node_id`'s tentative (pending-decision) key
/// set for the current/most recent run.
///
/// Written by this node whenever it passes an unseen item through; the host
/// unions it into [`committed_key`] on success or clears it on failure.
#[must_use]
pub fn tentative_key(node_id: &str) -> String {
    format!("dedup:{node_id}:tentative")
}

/// Loads the key set stored under `key` (a JSON array of strings) as a
/// [`HashSet`]. A missing key, a non-array value, or an array with non-string
/// elements yields an empty set rather than an error — `StateStore` state is
/// advisory bookkeeping this node degrades gracefully without, not a hard
/// dependency (a first run against a fresh store has nothing stored yet).
async fn load_key_set(ctx: &NodeContext<'_>, key: &str) -> Result<HashSet<String>> {
    let stored = ctx.caps.state.load(key).await?;
    let set: HashSet<String> = stored
        .as_ref()
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    tracing::debug!(key, set_len = set.len(), "{LOG_PREFIX} loaded key set");
    Ok(set)
}

/// Persists `set` under `key` as a JSON array of strings, sorted for a stable,
/// diffable on-disk representation (membership is exact-match either way, so
/// sort order carries no semantic meaning — it only makes manual inspection
/// and test assertions predictable).
async fn store_key_set(ctx: &NodeContext<'_>, key: &str, set: &HashSet<String>) -> Result<()> {
    let mut keys: Vec<String> = set.iter().cloned().collect();
    keys.sort_unstable();
    let value = Value::Array(keys.into_iter().map(Value::String).collect());
    tracing::debug!(key, set_len = set.len(), "{LOG_PREFIX} storing key set");
    ctx.caps.state.store(key, value).await
}

/// Extracts the dedup key string from an already-resolved `config.key` value,
/// or `None` when the key must fail open (null / absent / empty string).
///
/// A resolved string is used verbatim (never re-quoted), matching how the
/// `memory` node treats its own `key` field. A resolved non-string JSON value
/// (a bare number or boolean key, e.g. `config.key = "=item.id"` where
/// `item.id` is numeric) is canonicalized via its compact JSON rendering so it
/// still dedupes consistently — `123`, `true`, etc.
fn resolve_key(cfg: &Value) -> Option<String> {
    match cfg.get("key") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(other.to_string()),
    }
}

#[async_trait]
impl NodeExecutor for DedupNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let node_id = ctx.node.id.as_str();
        tracing::debug!(
            node = %node_id,
            input_len = ctx.input.len(),
            "{LOG_PREFIX} entering execute"
        );

        let committed = load_key_set(&ctx, &committed_key(node_id)).await?;

        // Keys newly passed THIS run, in encounter order — also doubles as the
        // within-batch duplicate check (rule: same key twice in one run's
        // input passes once, on first occurrence).
        let mut seen_this_run: HashSet<String> = HashSet::new();
        let mut passed = Vec::with_capacity(ctx.input.len());
        let mut diagnostics = Vec::new();

        for (index, input_item) in ctx.input.iter().enumerate() {
            let (cfg, diags) =
                crate::nodes::resolve_config_traced_for_item(&ctx, input_item.json.clone());
            diagnostics.extend(diags);

            let Some(key) = resolve_key(&cfg) else {
                tracing::warn!(
                    node = %node_id,
                    index,
                    "{LOG_PREFIX} resolved key is null/missing/empty — passing item through \
                     WITHOUT recording (fail-open, never silently dropped for a missing key)"
                );
                passed.push(input_item.clone());
                continue;
            };

            if committed.contains(&key) {
                tracing::debug!(
                    node = %node_id,
                    index,
                    key = %key,
                    "{LOG_PREFIX} key already committed — dropping item"
                );
                continue;
            }
            if !seen_this_run.insert(key.clone()) {
                tracing::debug!(
                    node = %node_id,
                    index,
                    key = %key,
                    "{LOG_PREFIX} key already seen earlier in this run's batch — dropping item"
                );
                continue;
            }

            tracing::debug!(
                node = %node_id,
                index,
                key = %key,
                "{LOG_PREFIX} key unseen — passing item through and staging as tentative"
            );
            passed.push(input_item.clone());
        }

        if !seen_this_run.is_empty() {
            let mut tentative = load_key_set(&ctx, &tentative_key(node_id)).await?;
            let added = seen_this_run
                .iter()
                .filter(|k| tentative.insert((*k).clone()))
                .count();
            tracing::debug!(
                node = %node_id,
                added,
                tentative_len = tentative.len(),
                "{LOG_PREFIX} writing merged tentative set"
            );
            store_key_set(&ctx, &tentative_key(node_id), &tentative).await?;
        }

        tracing::debug!(
            node = %node_id,
            emitted = passed.len(),
            dropped = ctx.input.len() - passed.len(),
            "{LOG_PREFIX} exiting execute"
        );
        Ok(NodeOutput::main(passed).with_diagnostics(diagnostics))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::Capabilities;
    use crate::caps::mock::mock_capabilities;
    use crate::data::Item;
    use crate::model::{Node, NodeKind};
    use serde_json::json;

    fn dedup_node(id: &str, config: Value) -> Node {
        Node {
            id: id.to_string(),
            kind: NodeKind::Dedup,
            type_version: 1,
            name: id.to_string(),
            config,
            ports: Vec::new(),
            position: None,
        }
    }

    async fn run_dedup(caps: &Capabilities, node: &Node, input: &[Item]) -> NodeOutput {
        let run = Value::Null;
        let ctx = NodeContext {
            node,
            input,
            run: &run,
            nodes: &Value::Null,
            caps,
        };
        DedupNode.execute(ctx).await.expect("execute")
    }

    #[tokio::test]
    async fn unseen_keys_pass_through_and_are_staged_tentative() {
        // Test 1 (spec): pre-seed committed with "a"; input a,b,c → only b,c
        // pass, and b,c land in tentative.
        let caps = mock_capabilities();
        caps.state
            .store(&committed_key("dd"), json!(["a"]))
            .await
            .unwrap();

        let node = dedup_node("dd", json!({ "key": "=item.id" }));
        let input = vec![
            Item::new(json!({ "id": "a" })),
            Item::new(json!({ "id": "b" })),
            Item::new(json!({ "id": "c" })),
        ];
        let out = run_dedup(&caps, &node, &input).await;

        let passed_ids: Vec<&str> = out
            .items
            .iter()
            .map(|i| i.json["id"].as_str().unwrap())
            .collect();
        assert_eq!(passed_ids, vec!["b", "c"], "a was already committed");
        assert_eq!(out.port, None, "dedup emits on the default main port");

        let tentative = caps
            .state
            .load(&tentative_key("dd"))
            .await
            .unwrap()
            .expect("tentative written");
        let mut tentative_arr: Vec<&str> = tentative
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        tentative_arr.sort_unstable();
        assert_eq!(tentative_arr, vec!["b", "c"]);
    }

    #[tokio::test]
    async fn committed_set_is_never_mutated_by_the_node_itself() {
        // Test 2 (spec): after the node runs, committed is UNCHANGED — proves
        // the node doesn't self-commit. The host (not this crate) is what
        // unions tentative into committed on run success, and clears
        // tentative on run failure — a crashed/failed run leaves committed
        // exactly as it was, so a retry can safely reprocess the same items.
        let caps = mock_capabilities();
        caps.state
            .store(&committed_key("dd"), json!(["a"]))
            .await
            .unwrap();

        let node = dedup_node("dd", json!({ "key": "=item.id" }));
        let input = vec![
            Item::new(json!({ "id": "a" })),
            Item::new(json!({ "id": "b" })),
        ];
        run_dedup(&caps, &node, &input).await;

        let committed = caps
            .state
            .load(&committed_key("dd"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(committed, json!(["a"]), "committed must be untouched");
    }

    #[tokio::test]
    async fn null_or_empty_key_fails_open_and_is_not_recorded() {
        // Test 3 (spec): a null/absent/empty key passes through and is NOT
        // added to tentative.
        let caps = mock_capabilities();
        let node = dedup_node("dd", json!({ "key": "=item.missing" }));
        let input = vec![
            Item::new(json!({ "other": 1 })),    // "=item.missing" resolves null
            Item::new(json!({ "missing": "" })), // resolves to an empty string
        ];
        let out = run_dedup(&caps, &node, &input).await;

        assert_eq!(out.items.len(), 2, "both fail-open items pass through");
        assert!(
            caps.state
                .load(&tentative_key("dd"))
                .await
                .unwrap()
                .is_none(),
            "a fail-open key must never be written to tentative"
        );
    }

    #[tokio::test]
    async fn two_dedup_nodes_keep_separate_committed_and_tentative_sets() {
        // Test 4 (spec): two dedup nodes (different node ids) in one flow keep
        // separate sets — the node-id discriminator in the StateStore key.
        let caps = mock_capabilities();
        caps.state
            .store(&committed_key("dd1"), json!(["shared"]))
            .await
            .unwrap();

        let node1 = dedup_node("dd1", json!({ "key": "=item.id" }));
        let node2 = dedup_node("dd2", json!({ "key": "=item.id" }));
        let input = vec![Item::new(json!({ "id": "shared" }))];

        let out1 = run_dedup(&caps, &node1, &input).await;
        assert!(out1.items.is_empty(), "dd1 already committed \"shared\"");

        let out2 = run_dedup(&caps, &node2, &input).await;
        assert_eq!(
            out2.items.len(),
            1,
            "dd2's committed set is independent of dd1's and has never seen \"shared\""
        );

        let dd2_tentative = caps
            .state
            .load(&tentative_key("dd2"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(dd2_tentative, json!(["shared"]));
        assert!(
            caps.state
                .load(&tentative_key("dd1"))
                .await
                .unwrap()
                .is_none(),
            "dd1 emitted nothing this run, so it wrote no tentative keys"
        );
    }

    #[tokio::test]
    async fn same_key_twice_in_one_run_passes_once_only() {
        // Test 5 (spec, pinned convention): a duplicate key within one run's
        // input batch passes on its first occurrence only; later duplicates
        // are dropped by the same rule as an already-committed key.
        let caps = mock_capabilities();
        let node = dedup_node("dd", json!({ "key": "=item.id" }));
        let input = vec![
            Item::new(json!({ "id": "x", "n": 1 })),
            Item::new(json!({ "id": "x", "n": 2 })),
            Item::new(json!({ "id": "y", "n": 3 })),
        ];
        let out = run_dedup(&caps, &node, &input).await;

        assert_eq!(out.items.len(), 2, "the second \"x\" is dropped");
        assert_eq!(out.items[0].json["n"], 1, "the FIRST occurrence wins");
        assert_eq!(out.items[1].json["id"], "y");

        let tentative = caps
            .state
            .load(&tentative_key("dd"))
            .await
            .unwrap()
            .unwrap();
        let mut arr: Vec<&str> = tentative
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        arr.sort_unstable();
        assert_eq!(arr, vec!["x", "y"], "x is staged exactly once, not twice");
    }

    #[tokio::test]
    async fn a_key_already_present_in_stale_tentative_is_preserved_across_runs() {
        // A prior run's tentative write survives into the NEXT run's load —
        // this node only ever unions into tentative, it never overwrites it
        // wholesale. (Whether the host should have cleared stale tentative
        // before this run is a host-side concern this node has no opinion on.)
        let caps = mock_capabilities();
        caps.state
            .store(&tentative_key("dd"), json!(["stale"]))
            .await
            .unwrap();

        let node = dedup_node("dd", json!({ "key": "=item.id" }));
        let input = vec![Item::new(json!({ "id": "fresh" }))];
        run_dedup(&caps, &node, &input).await;

        let tentative = caps
            .state
            .load(&tentative_key("dd"))
            .await
            .unwrap()
            .unwrap();
        let mut arr: Vec<&str> = tentative
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        arr.sort_unstable();
        assert_eq!(arr, vec!["fresh", "stale"]);
    }

    #[tokio::test]
    async fn non_string_resolved_key_is_canonicalized() {
        // A key expression that resolves to a JSON number still dedupes.
        let caps = mock_capabilities();
        let node = dedup_node("dd", json!({ "key": "=item.id" }));
        let input = vec![Item::new(json!({ "id": 7 })), Item::new(json!({ "id": 7 }))];
        let out = run_dedup(&caps, &node, &input).await;
        assert_eq!(out.items.len(), 1, "numeric key 7 dedupes against itself");
    }

    #[tokio::test]
    async fn empty_input_is_a_no_op_and_writes_nothing() {
        let caps = mock_capabilities();
        let node = dedup_node("dd", json!({ "key": "=item.id" }));
        let out = run_dedup(&caps, &node, &[]).await;
        assert!(out.items.is_empty());
        assert!(
            caps.state
                .load(&tentative_key("dd"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn missing_key_config_fails_open_for_every_item() {
        // No `key` in config at all (e.g. a bare/default config in a generic
        // dispatch test) — same fail-open path as a key that resolves null.
        let caps = mock_capabilities();
        let node = dedup_node("dd", Value::Null);
        let input = vec![Item::new(json!({ "id": "a" }))];
        let out = run_dedup(&caps, &node, &input).await;
        assert_eq!(out.items.len(), 1);
        assert!(
            caps.state
                .load(&tentative_key("dd"))
                .await
                .unwrap()
                .is_none()
        );
    }
}
