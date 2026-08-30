//! Whether an authored graph is a procedure or a one-off.
//!
//! The authoring prompt asks for a graph that is generic with declared inputs:
//! *"read it in config rather than pasting the literal. A graph with the value
//! baked in is a graph that works once."* Nothing checked that, and nothing
//! kept the result — so a graph authored for a goal, which then achieved it,
//! was thrown away and re-authored from scratch the next time the same kind of
//! thing was asked.
//!
//! Keeping it needs a gate, because keeping *every* one is worse than keeping
//! none: a catalogue full of graphs that each match one task makes selection
//! harder, not easier, and every row is a row the planner reads.
//!
//! # The gate is exact, not a judgement
//!
//! Authoring returns two things — the graph, and the concrete input values for
//! this run. So the question "did it bake the specifics in" has a precise
//! answer: **does a value it handed us as an input appear as a literal inside a
//! node's config?**
//!
//! ```text
//! inputs: { "repo": "acme/thing" }
//!
//! reusable   { "prompt": "review the PRs on =run.inputs.repo" }
//! one-off    { "prompt": "review the PRs on acme/thing" }
//! ```
//!
//! Both run. Both may satisfy the goal. Only the first is worth keeping, and
//! telling them apart needs no model and no guessing — which matters, because a
//! fuzzy gate on a store that grows forever is a store that fills with
//! near-misses.

use serde_json::Value;
use tinyflows::model::WorkflowGraph;

/// Length alone at which a value is distinctive enough to be evidence.
const LONG_ENOUGH: usize = 8;

/// Characters that make a short value distinctive anyway.
///
/// A path, a repository, an id, an address — `acme/thing`, `/docs/q3.pdf`,
/// `PROJ-1234`, `ops@example.com` all carry one. A bare short word does not.
const DISTINCTIVE_CHARS: [char; 6] = ['/', '.', ':', '@', '_', '-'];

/// Whether finding this value in a config proves anything.
///
/// `"1"`, `"true"`, `"main"` are values an input can legitimately carry *and* a
/// node can legitimately contain for unrelated reasons — `main` is the default
/// port name on every edge in the graph. Treating those as pasted would refuse
/// to keep perfectly reusable procedures, and a gate that fires on noise is one
/// nobody trusts.
pub(crate) fn distinctive(value: &str) -> bool {
    let length = value.chars().count();
    // A digit only counts alongside some length: `"1"` proves nothing and, via
    // the substring test, would match any config containing that character —
    // reporting a paste and discarding a perfectly reusable procedure.
    length >= LONG_ENOUGH
        || value.contains(DISTINCTIVE_CHARS)
        || (length >= 4 && value.chars().any(|c| c.is_ascii_digit()))
}

/// Input values this graph pasted into a node instead of reading.
///
/// Empty means it is reusable: every specific it was given arrives through a
/// declared input, so the same graph serves the next goal of this shape.
///
/// Only leaf strings are examined. A value appearing as a *key* is not evidence
/// — a config may legitimately be keyed by something the goal also named.
#[must_use]
pub fn baked_in(graph: &WorkflowGraph, inputs: &serde_json::Map<String, Value>) -> Vec<String> {
    let distinctive: Vec<&str> = inputs
        .values()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|value| distinctive(value))
        .collect();
    if distinctive.is_empty() {
        return Vec::new();
    }

    let mut found: Vec<String> = Vec::new();
    for node in &graph.nodes {
        let mut leaves = Vec::new();
        collect_strings(&node.config, &mut leaves);
        for leaf in leaves {
            for value in &distinctive {
                // Expressions are scanned too, but only their QUOTED
                // literals: `=.run.inputs.repo` reads the value from the
                // run and is fine; `="review acme/thing"` welded it in —
                // and since generated prompts are all expressions now, an
                // expression-shaped paste is the common shape, not the
                // exception.
                let pasted = if leaf.starts_with('=') {
                    quoted_literals(&leaf).iter().any(|lit| lit.contains(value))
                } else {
                    leaf.contains(value)
                };
                if pasted && !found.iter().any(|f| f == value) {
                    found.push((*value).to_string());
                }
            }
        }
    }
    found
}

/// A stable id for a graph's runnable shape.
///
/// Derived rather than counted, so the same procedure arrived at twice
/// converges on one stored workflow instead of accumulating near-duplicates —
/// and so keeping needs no read of what already exists.
///
/// The same digest the authoring fingerprint uses, for the same reason: nodes,
/// edges and declared inputs are what runs; the name and description are prose
/// a later pass may improve without making it a different procedure.
#[must_use]
pub fn shape_id(graph: &WorkflowGraph) -> String {
    format!("learned-{}", digest_hex(&shape_bytes(graph)))
}

/// The canonical bytes an identity is derived from.
pub(crate) fn shape_bytes(graph: &WorkflowGraph) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "nodes": &graph.nodes,
        "edges": &graph.edges,
        "inputs": &graph.inputs,
    }))
    .unwrap_or_default()
}

/// FNV-1a over the bytes, 64 bits, rendered as 16 hex chars.
///
/// Not `DefaultHasher`: these digests become **persisted identifiers** —
/// workflow ids, lineage keys, exclusion-list signatures — and `DefaultHasher`
/// is explicitly unstable across Rust releases, so a toolchain upgrade would
/// silently stop identical work converging and orphan every stored score. The
/// old 28-bit truncation also put birthday collisions within reach of a few
/// tens of thousands of records; 64 bits does not. FNV-1a is fixed forever,
/// fits in six lines, and needs no dependency.
/// The quoted string literals of a jq expression, unescaped enough to
/// substring-search: `="a \"b\"" + .x` yields `a "b"`.
fn quoted_literals(expression: &str) -> Vec<String> {
    let mut literals = Vec::new();
    let mut current: Option<String> = None;
    let mut chars = expression.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => match current.take() {
                Some(literal) => literals.push(literal),
                None => current = Some(String::new()),
            },
            '\\' if current.is_some() => {
                if let (Some(literal), Some(escaped)) = (current.as_mut(), chars.next()) {
                    literal.push(match escaped {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => other,
                    });
                }
            }
            other => {
                if let Some(literal) = current.as_mut() {
                    literal.push(other);
                }
            }
        }
    }
    literals
}

pub(crate) fn digest_hex(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Every string leaf in a config, keys excluded.
fn collect_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => items.iter().for_each(|item| collect_strings(item, out)),
        Value::Object(map) => map.values().for_each(|v| collect_strings(v, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tinyflows::model::{Node, NodeKind};

    fn graph_with(config: Value) -> WorkflowGraph {
        WorkflowGraph {
            schema_version: 1,
            id: Some("g".into()),
            name: "g".into(),
            inputs: Vec::new(),
            agents: Vec::new(),
            nodes: vec![Node {
                id: "step".into(),
                kind: NodeKind::Agent,
                type_version: 1,
                name: "step".into(),
                config,
                ports: Vec::new(),
                position: None,
            }],
            edges: Vec::new(),
        }
    }

    fn inputs(pairs: &[(&str, &str)]) -> serde_json::Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), json!(v)))
            .collect()
    }

    #[test]
    fn a_value_read_through_an_input_is_reusable() {
        let graph = graph_with(json!({ "prompt": "review the PRs on =run.inputs.repo" }));
        assert!(baked_in(&graph, &inputs(&[("repo", "acme/thing")])).is_empty());
    }

    #[test]
    fn the_same_value_pasted_in_is_a_one_off() {
        let graph = graph_with(json!({ "prompt": "review the PRs on acme/thing" }));
        assert_eq!(
            baked_in(&graph, &inputs(&[("repo", "acme/thing")])),
            vec!["acme/thing"]
        );
    }

    #[test]
    fn it_looks_inside_nested_config_not_just_the_top_level() {
        let graph = graph_with(json!({
            "args": { "targets": ["/docs/q3.pdf", "=run.inputs.other"] }
        }));
        assert_eq!(
            baked_in(&graph, &inputs(&[("path", "/docs/q3.pdf")])),
            vec!["/docs/q3.pdf"]
        );
    }

    #[test]
    fn an_expression_reading_the_value_by_path_is_not_a_paste() {
        // `=run.inputs.repo | ascii_downcase` resolves the value at run time —
        // the graph works for the next repo too.
        let graph = graph_with(json!({ "prompt": "=run.inputs.repo | ascii_downcase" }));
        assert!(baked_in(&graph, &inputs(&[("repo", "acme/thing")])).is_empty());
    }

    #[test]
    fn a_value_welded_into_an_expressions_quoted_literal_is_a_paste() {
        // `="acme/thing"` evaluates to exactly the pasted text: expression
        // syntax around a literal changes nothing about its reusability. Since
        // recipe lowering made every generated prompt an expression, this is
        // the common shape of a paste, not an edge case.
        let graph = graph_with(json!({ "prompt": "=\"review acme/thing directly\"" }));
        assert_eq!(
            baked_in(&graph, &inputs(&[("repo", "acme/thing")])),
            vec!["acme/thing".to_string()]
        );
    }

    #[test]
    fn plain_short_words_prove_nothing_and_are_not_evidence() {
        // `main` is the default port name on every edge in the graph, so a node
        // containing it says nothing about where the input went. A gate that
        // fires on that refuses perfectly reusable procedures.
        let graph = graph_with(json!({ "branch": "main", "mode": "on" }));
        assert!(baked_in(&graph, &inputs(&[("branch", "main"), ("mode", "on")])).is_empty());
    }

    #[test]
    fn a_bare_short_digit_is_not_evidence() {
        // "1" appears in half of all configs; treating it as a paste would
        // refuse reusable procedures on noise.
        let graph = graph_with(json!({ "max_items": "10", "prompt": "top 1 result" }));
        assert!(baked_in(&graph, &inputs(&[("n", "1"), ("count", "10")])).is_empty());
    }

    #[test]
    fn a_short_value_with_structure_is_still_evidence() {
        // Short but unmistakable: nothing else in a config is `a/b` or has a
        // ticket number in it by coincidence.
        for (key, value) in [("repo", "a/b"), ("ticket", "P-91")] {
            let graph = graph_with(json!({ "prompt": format!("do {value}") }));
            assert_eq!(
                baked_in(&graph, &inputs(&[(key, value)])),
                vec![value.to_string()],
                "{value} should read as pasted"
            );
        }
    }

    #[test]
    fn a_key_that_matches_is_not_a_paste() {
        // Configs are keyed by field names, and a goal may name one. Only the
        // values a node would send are evidence.
        let graph = graph_with(json!({ "acme/thing": "=run.inputs.repo" }));
        assert!(baked_in(&graph, &inputs(&[("repo", "acme/thing")])).is_empty());
    }

    #[test]
    fn a_graph_with_no_inputs_at_all_is_reusable_by_default() {
        // "summarise today's pull requests" has no parameters. Nothing was
        // given, so nothing could have been baked in.
        let graph = graph_with(json!({ "prompt": "summarise today's pull requests" }));
        assert!(baked_in(&graph, &inputs(&[])).is_empty());
    }

    #[test]
    fn the_digest_is_the_documented_algorithm_not_a_std_implementation_detail() {
        // Pinned to FNV-1a's published test vectors: if this fails, persisted
        // identifiers changed and every stored score is orphaned.
        assert_eq!(digest_hex(b""), "cbf29ce484222325");
        assert_eq!(digest_hex(b"a"), "af63dc4c8601ec8c");
    }

    #[test]
    fn every_pasted_value_is_reported_not_only_the_first() {
        // A caller renders these into an explanation of why a graph was not
        // kept, and one at a time turns that into a conversation.
        let graph = graph_with(json!({
            "prompt": "review acme/thing at /docs/q3.pdf"
        }));
        let found = baked_in(
            &graph,
            &inputs(&[("repo", "acme/thing"), ("path", "/docs/q3.pdf")]),
        );
        assert_eq!(found.len(), 2, "{found:?}");
    }
}
