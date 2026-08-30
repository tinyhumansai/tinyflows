//! Authoring when nothing stored fits: a recipe in, a lowered graph out.
//!
//! The model never writes graph syntax — see [`super::recipe`] for why and
//! for the surface it writes instead. What stays here is the conversation:
//! bounded feedback rounds, and the gates every candidate walks before it
//! may become an attempt. The gates run on the LOWERED graph; by
//! construction they should all pass, and a construction bug surfacing as a
//! refusal rather than a run-time null is exactly why they still run.

use serde_json::Value;
use tinyflows::caps::Capabilities;
use tinyflows::model::WorkflowGraph;
use tinyflows::store::HostPolicy;
use tinyflows::validate::validate_all;

use super::{Attempt, IntakeError, Result, ask, recipe};
use crate::contracts::{Approach, Goal, Tier};
use crate::host::HostFacts;
use recipe::Callable;

/// Write a graph for `goal`, grounded on the engine's own node catalogue.
///
/// # Errors
/// When inference fails, the reply holds no graph, or the graph does not
/// validate. An invalid graph is never returned: the caller would hand it
/// straight to `compile`, and the resulting failure would be attributed to the
/// work rather than to the authoring.
pub async fn author(
    goal: &Goal,
    facts: &HostFacts,
    callables: &[Callable],
    policy: &dyn HostPolicy,
    past: &str,
    caps: &Capabilities,
    conn: Option<&str>,
) -> Result<Attempt> {
    let permitted = facts.render();
    // The callable listing goes beside what the host permits, because it is
    // the same kind of fact: these exist, the rest do not. A plan naming one
    // that is not here is refused at intake, not discovered mid-run.
    let offered = recipe::render_callables(callables);
    let user = format!(
        "# Goal\n{}{}{}{past}",
        goal.text.trim(),
        if permitted.is_empty() {
            String::new()
        } else {
            format!("\n\n{permitted}")
        },
        if offered.is_empty() {
            String::new()
        } else {
            format!("\n\n{offered}")
        }
    );

    // The gates below produce readable refusals on purpose, and this loop is
    // where they earn it: a refused graph goes back to the model with the
    // refusal, once per round, rather than costing the whole episode. Bounded,
    // because a model that cannot fix its graph in two more tries is telling
    // us the answer.
    //
    // A reply that never became an answer — no JSON object, a transport
    // failure — is retried too, but with the prompt unchanged: there is no
    // graph to give feedback on, and a resample is the whole remedy.
    let mut prompt = user;
    let mut last: Option<IntakeError> = None;
    for _ in 0..ROUNDS {
        let answer = match ask(caps, conn, Tier::Author, recipe::SYSTEM, &prompt).await {
            Ok(answer) => answer,
            Err(err) => {
                last = Some(err);
                continue;
            }
        };
        match gated(&answer, facts, callables, policy) {
            Ok(attempt) => return Ok(attempt),
            Err(err) => {
                prompt = format!(
                    "{prompt}\n\n# Your previous graph was refused — fix exactly this\n\
                     {err}\n\nReturn the corrected, complete JSON reply."
                );
                last = Some(err);
            }
        }
    }
    Err(last.unwrap_or_else(|| IntakeError::Inference("the author was never asked".to_string())))
}

/// How many replies the author gets before the failure is the answer.
const ROUNDS: usize = 3;

/// One reply through every gate, or why it was refused.
fn gated(
    answer: &Value,
    facts: &HostFacts,
    callables: &[Callable],
    policy: &dyn HostPolicy,
) -> Result<Attempt> {
    let (graph, mut inputs, why) = recipe::lower(answer, callables)?;

    // Every failure at once, not the first. A model handed one error fixes it
    // and returns with the next; handed all four it fixes all four.
    let problems = validate_all(&graph);
    if !problems.is_empty() {
        return Err(IntakeError::Invalid(
            problems
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }

    // The single most common authoring mistake, across every model tried: a
    // binding path inside prose — `"about: .run.inputs.topic"` — which the
    // engine reads as those literal characters, so the step runs on garbage
    // and reports success. Mechanically detectable, so it is refused here and
    // fixed through the feedback loop rather than found by a judge two
    // minutes and one model call later.
    let prose = prose_bindings(&graph);
    if !prose.is_empty() {
        return Err(IntakeError::Invalid(prose.join("; ")));
    }

    // Three gates, and the order is cost. `validate_all` is structural and
    // free. `HostFacts::check` is our own reading of the machine's config.
    // `check_graph` is the host's, which may know things we were not told —
    // it runs last because it is the one that can reach outside this process.
    let refused = facts.check(&graph);
    if !refused.is_empty() {
        return Err(IntakeError::Unsupported(refused.join("; ")));
    }
    if let Err(err) = policy.check_graph(graph.id.as_deref().unwrap_or("authored"), &graph) {
        return Err(IntakeError::Unsupported(err.to_string()));
    }

    // The same both-direction input check a *selection* gets in `bind`, for
    // the author's own declarations. A model that declares a required input
    // and supplies no value has written a graph that dies at the engine's
    // door — the failure this gate turns into a feedback round cost a whole
    // attempt (harness sessions included) every time it slipped through.
    for declared in &graph.inputs {
        if !declared.required {
            continue;
        }
        let filled = inputs
            .get(&declared.name)
            .is_some_and(|value| !value.is_null() && value.as_str() != Some(""));
        if !filled {
            return Err(IntakeError::Invalid(format!(
                "the graph declares required input `{}` but the reply's `inputs`                  supplies no value for it — supply one, or make it optional, or                  drop the declaration",
                declared.name
            )));
        }
    }
    // The other direction is a trim, not a refusal, exactly as in `bind`:
    // the engine rejects undeclared keys before any node executes.
    inputs.retain(|name, _| graph.inputs.iter().any(|d| d.name == *name));

    Ok(Attempt {
        // See `select`: continuing is the loop's call, not intake's.
        resume: None,
        approach: Approach::Authored {
            why,
            fingerprint: fingerprint(&graph),
        },
        graph,
        inputs,
        // Filled by `decide`, which is what knows what the planner was shown.
        lessons_shown: Vec::new(),
    })
}

/// Config strings that embed a binding path in prose instead of being one.
///
/// A string that does not start with `=` is a literal, whole. One that
/// mentions `run.inputs.`, `run.trigger.` or `nodes.<id>.` inside prose was
/// almost certainly meant to interpolate — and will instead hand the model,
/// the tool or the request those exact characters. Expression strings
/// (leading `=`) are exempt: `="about \(.run.inputs.topic)"` legitimately
/// contains the path.
fn prose_bindings(graph: &WorkflowGraph) -> Vec<String> {
    const PATHS: [&str; 5] = ["run.inputs.", "run.trigger.", "=run.", "=nodes.", ".nodes."];

    fn scan(node: &str, field: &str, value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(s) if !s.starts_with('=') => {
                if PATHS.iter().any(|p| s.contains(p)) {
                    out.push(format!(
                        "node `{node}` config `{field}` embeds a binding path in literal \
                         text, which the engine passes through as those exact characters. \
                         Make the whole string one expression instead: \
                         =\"… \\(.run.inputs.name) …\""
                    ));
                }
            }
            Value::Object(map) => {
                for (key, nested) in map {
                    scan(node, key, nested, out);
                }
            }
            Value::Array(items) => {
                for nested in items {
                    scan(node, field, nested, out);
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    for node in &graph.nodes {
        scan(&node.id, "config", &node.config, &mut out);
    }
    out
}

/// A digest of the graph's runnable shape.
///
/// Nodes, edges and declared inputs — not the name, not the description. Two
/// graphs that run identically and differ in prose are the same attempt, and
/// the whole point of the exclusion list is that the second one is recognised
/// as a repeat rather than counted as a fresh idea. Inputs are in because a
/// graph that requires a value behaves differently from one that does not,
/// even when every node matches.
fn fingerprint(graph: &WorkflowGraph) -> String {
    // The stable digest, because this string is persisted in ledger rows as
    // the exclusion-list signature — see `reuse::digest_hex` on why not
    // `DefaultHasher`.
    crate::reuse::digest_hex(&crate::reuse::shape_bytes(graph))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_refused_graph_goes_back_to_the_model_with_the_refusal() {
        use std::sync::Mutex;

        use tinyflows::caps::LlmProvider;
        use tinyflows::caps::mock::mock_capabilities;

        /// First reply: a graph with no trigger. Second: a valid one — but
        /// only if the follow-up prompt actually carries the refusal.
        struct Corrigible {
            prompts: Mutex<Vec<String>>,
        }

        #[async_trait::async_trait]
        impl LlmProvider for Corrigible {
            async fn complete(
                &self,
                request: Value,
                _conn: Option<&str>,
            ) -> tinyflows::error::Result<Value> {
                let shown = request["messages"][1]["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let mut prompts = self.prompts.lock().expect("prompt log");
                prompts.push(shown.clone());
                if prompts.len() == 1 {
                    // No steps: refused by the lowering, must come back.
                    return Ok(serde_json::json!({ "why": "broken", "inputs": {} }));
                }
                assert!(
                    shown.contains("refused"),
                    "the retry prompt must carry the refusal, got: {shown}"
                );
                Ok(serde_json::json!({
                    "why": "fixed",
                    "inputs": {},
                    "steps": [{ "id": "do_it", "ask": "Do the thing directly." }]
                }))
            }
        }

        #[derive(Debug, Default)]
        struct Permissive;
        impl HostPolicy for Permissive {}

        let provider = std::sync::Arc::new(Corrigible {
            prompts: Mutex::new(Vec::new()),
        });
        let caps = Capabilities {
            llm: provider.clone(),
            ..mock_capabilities()
        };
        let attempt = author(
            &Goal::new("do the thing"),
            &HostFacts::unknown(),
            &[],
            &Permissive,
            "",
            &caps,
            None,
        )
        .await
        .expect("the corrected graph must land");
        assert_eq!(attempt.graph.name, "fixed");
        assert_eq!(attempt.graph.nodes[1].id, "do_it");
        assert_eq!(provider.prompts.lock().expect("prompt log").len(), 2);
    }

    #[tokio::test]
    async fn an_unsupplied_required_input_goes_back_to_the_model() {
        use std::sync::Mutex;

        use tinyflows::caps::LlmProvider;
        use tinyflows::caps::mock::mock_capabilities;

        /// Declares a required `topic` both times; supplies a value only when
        /// the follow-up prompt carries the refusal.
        struct Forgetful {
            prompts: Mutex<Vec<String>>,
        }

        #[async_trait::async_trait]
        impl LlmProvider for Forgetful {
            async fn complete(
                &self,
                request: Value,
                _conn: Option<&str>,
            ) -> tinyflows::error::Result<Value> {
                let shown = request["messages"][1]["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let mut prompts = self.prompts.lock().expect("prompt log");
                prompts.push(shown.clone());
                let inputs = if prompts.len() == 1 {
                    serde_json::json!({ "extraneous": "trimmed anyway" })
                } else {
                    assert!(
                        shown.contains("topic"),
                        "the retry names the unsupplied input: {shown}"
                    );
                    serde_json::json!({ "topic": "flash models", "extraneous": "still here" })
                };
                Ok(serde_json::json!({
                    "why": "test",
                    "declared": [{ "name": "topic", "description": "what about", "required": true }],
                    "inputs": inputs,
                    "steps": [{ "id": "write", "ask": "Write it." }]
                }))
            }
        }

        #[derive(Debug, Default)]
        struct Permissive;
        impl HostPolicy for Permissive {}

        let provider = std::sync::Arc::new(Forgetful {
            prompts: Mutex::new(Vec::new()),
        });
        let caps = Capabilities {
            llm: provider.clone(),
            ..mock_capabilities()
        };
        let attempt = author(
            &Goal::new("do the thing"),
            &HostFacts::unknown(),
            &[],
            &Permissive,
            "",
            &caps,
            None,
        )
        .await
        .expect("the corrected inputs must land");
        assert_eq!(attempt.inputs["topic"], "flash models");
        assert!(
            !attempt.inputs.contains_key("extraneous"),
            "undeclared inputs are trimmed: {:?}",
            attempt.inputs
        );
        assert_eq!(provider.prompts.lock().expect("prompt log").len(), 2);
    }

    #[test]
    fn a_binding_path_inside_prose_is_refused_with_the_remedy() {
        use tinyflows::model::{Edge, Node, NodeKind};

        let graph = WorkflowGraph {
            schema_version: 1,
            name: "poem".into(),
            nodes: vec![
                Node {
                    id: "start".into(),
                    kind: NodeKind::Trigger,
                    type_version: 1,
                    name: "manual".into(),
                    config: serde_json::json!({ "trigger_kind": "manual" }),
                    ports: Vec::new(),
                    position: None,
                },
                Node {
                    id: "poet".into(),
                    kind: NodeKind::Agent,
                    type_version: 1,
                    name: "poet".into(),
                    config: serde_json::json!({
                        "prompt": "Write a poem about: .run.inputs.topic"
                    }),
                    ports: Vec::new(),
                    position: None,
                },
            ],
            edges: vec![Edge {
                from_node: "start".into(),
                from_port: "main".into(),
                to_node: "poet".into(),
                to_port: "main".into(),
            }],
            ..WorkflowGraph::default()
        };

        let found = prose_bindings(&graph);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("poet"), "names the node: {}", found[0]);

        // A node path in prose is the same mistake with a different root.
        let mut node_path = graph.clone();
        node_path.nodes[1].config =
            serde_json::json!({ "prompt": "Summarise .nodes.fetch.item.json.body" });
        assert_eq!(prose_bindings(&node_path).len(), 1);

        // The legitimate form is exempt: the whole string is an expression.
        let mut fixed = graph;
        fixed.nodes[1].config =
            serde_json::json!({ "prompt": "=\"Write a poem about \\(.run.inputs.topic)\"" });
        assert!(prose_bindings(&fixed).is_empty());
    }

    #[test]
    fn a_graph_with_no_trigger_is_refused_rather_than_returned() {
        // Not reachable through `author` without a provider, so the invariant
        // is asserted against the validator this module gates on.
        let graph = WorkflowGraph {
            name: "no trigger".to_string(),
            ..WorkflowGraph::default()
        };
        assert!(
            !validate_all(&graph).is_empty(),
            "an empty graph must not validate — intake gates on exactly this"
        );
    }
}
