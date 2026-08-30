//! Checks that run before an authoring write lands.
//!
//! [`validate`](crate::validate) answers "would this compile" — no trigger, an
//! edge to a node that is not there. That is a real bar and it is not the one
//! authors keep failing. The graphs that cost people an afternoon *do* compile:
//! they have a binding that resolves to null at run time, so a step executes
//! with an empty value and the run reports success having done nothing.
//!
//! Nothing downstream catches that. A null is a legal value, so the engine has
//! no complaint; the run record shows every node green. The only place it can be
//! caught is here, before the write, while there is still an author on the other
//! end to tell.
//!
//! Two rules the gates hold themselves to:
//!
//! - **Refuse only what is *guaranteed* wrong.** A gate that fires on a graph
//!   that would have worked costs an author their edit and teaches them to
//!   distrust the tool. Everything merely suspicious belongs in a dry run's
//!   diagnostics, which advise rather than refuse.
//! - **Say what to do.** Every message names the node, the binding, and the
//!   correction. The reader is often an agent with one round trip to spend.
//!
//! # What is *not* here
//!
//! Anything that depends on a host's own vocabulary. Which harnesses exist,
//! which tool slugs resolve, which integrations are installed — a gate over any
//! of those would have to hard-code a host, which this crate does not do. A host
//! adds its own by implementing
//! [`HostPolicy::check_graph`](crate::store::HostPolicy::check_graph), whose
//! default is exactly [`failures`] below.

use crate::bindings::{self, collect_expressions, parse_node_binding, reads_as_prose};
use crate::model::{NodeKind, WorkflowGraph};

/// Every gate failure in `graph`, collected rather than short-circuited.
///
/// One round trip then tells an author everything wrong with what they wrote,
/// which matters most when the author is an agent editing over a tool call.
///
/// An empty result is a pass.
#[must_use]
pub fn failures(graph: &WorkflowGraph) -> Vec<String> {
    let mut failures = agent_prompt_failures(graph);
    failures.extend(binding_failures(graph));
    failures.extend(agent_schema_failures(graph));
    failures.extend(code_language_failures(graph));
    failures
}

/// `code` nodes whose language the engine will not read the way it was written.
///
/// The engine matches the literal string `"python"` and treats *everything else*
/// as JavaScript — silently. So `"language": "python3"` runs a Python program
/// through node, and `"language": "shell"` runs a shell script through node.
/// Both fail with a syntax error from an interpreter the author never named,
/// which is among the least helpful failures a run can produce.
///
/// Refused here rather than documented, because documentation does not stop a
/// plausible spelling.
fn code_language_failures(graph: &WorkflowGraph) -> Vec<String> {
    /// The two the engine actually distinguishes.
    const ACCEPTED: [&str; 2] = ["javascript", "python"];
    /// Spellings that mean "a shell", which a `code` node cannot run at all.
    const SHELL_SPELLINGS: [&str; 3] = ["shell", "sh", "bash"];

    let mut failures = Vec::new();
    for node in &graph.nodes {
        if node.kind != NodeKind::Code {
            continue;
        }
        let Some(language) = node.config.get("language").and_then(|v| v.as_str()) else {
            // Absent is legal and means JavaScript, which the engine's own
            // default already says.
            continue;
        };
        if ACCEPTED.contains(&language) {
            continue;
        }
        // Worth its own sentence: an author reaching for a shell has a node kind
        // that does exactly that, and the generic message would send them
        // looking for a spelling of `language` that does not exist.
        let hint = if SHELL_SPELLINGS.contains(&language.trim().to_ascii_lowercase().as_str()) {
            " A `code` node cannot run shell: use a `shell` node, which takes an interpreter, a \
             working directory, and an environment."
        } else {
            ""
        };
        failures.push(format!(
            "node '{}': `language` is `{language}`, which this engine does not recognise — it \
             matches only the exact strings `javascript` and `python`, and silently treats \
             anything else as JavaScript. Your program would be run through node and fail with a \
             syntax error naming an interpreter you did not choose.{hint}",
            node.id
        ));
    }
    failures
}

/// Agent nodes whose `prompt` is prose written as an expression.
///
/// The node would run with an empty instruction — on a host that dispatches a
/// whole agent session per node, that is a session started with nothing to do.
fn agent_prompt_failures(graph: &WorkflowGraph) -> Vec<String> {
    let mut failures = Vec::new();
    for node in &graph.nodes {
        if node.kind != NodeKind::Agent {
            continue;
        }
        // A node that declares real `messages` never runs on `prompt` at all —
        // both completion paths fall through to the messages array once the
        // prompt resolves to null. Refusing the graph for a vestigial `prompt`
        // beside real messages would be a refusal with no failure behind it.
        let messages_supply_the_turn = node
            .config
            .get("messages")
            .and_then(|v| v.as_array())
            .is_some_and(|entries| !entries.is_empty());
        if messages_supply_the_turn {
            continue;
        }
        // `instruction` is the alias hosts commonly use for the same field, and
        // the engine accepts it, so it has to be checked too.
        for key in ["prompt", "instruction"] {
            let Some(text) = node.config.get(key).and_then(|v| v.as_str()) else {
                continue;
            };
            if !crate::expr::is_expression(text) {
                continue;
            }
            if reads_as_prose(text[1..].trim()) {
                failures.push(format!(
                    "node '{}': `{key}` (`{text}`) reads as an instruction written as a \
                     `=`-expression, not as a jq program. `=` does not interpolate — the whole \
                     thing resolves to null and the node runs with an empty prompt. Fix: drop the \
                     leading `=` and write the instruction plainly, referring to upstream data \
                     with a separate `=` binding.",
                    node.id
                ));
            }
        }
    }
    failures
}

/// Bindings that read a node's output through the wrong shape.
fn binding_failures(graph: &WorkflowGraph) -> Vec<String> {
    let mut failures = Vec::new();
    for node in &graph.nodes {
        for (location, expr) in collect_expressions(&node.config) {
            let Some(binding) = parse_node_binding(&expr) else {
                continue;
            };
            // A binding to a node that does not exist is the engine's to
            // report, and it already does.
            let Some(target) = bindings::node_of(graph, &binding.node_id) else {
                continue;
            };
            // `.item.text` / `.item.raw` / `.item.json` address the envelope
            // itself, which is the correct way to read an agent's completion
            // text or an untouched response — not a missing `.json`.
            if bindings::reads_the_envelope_itself(&binding.field_path) {
                continue;
            }
            if bindings::wraps_output(&target.kind) && !binding.through_envelope {
                failures.push(format!(
                    "node '{}': `{location}` (`{expr}`) reads `.item.{path}` from {article} node \
                     `{target_id}`, whose output is wrapped as {{json, text, raw}} — so this \
                     resolves to null at run time and the step gets nothing. Fix: \
                     `=nodes.{target_id}.item.json.{path}`.",
                    node.id,
                    path = binding.field_path,
                    article = bindings::kind_article(&target.kind),
                    target_id = binding.node_id,
                ));
            }
        }
    }
    failures
}

/// Tool-call arguments bound to an agent field that is not addressable.
///
/// An `agent` node's structured output is exactly what its `output_parser.schema`
/// declares — that is the shape the node's own sub-port validates and repairs a
/// completion into. Reading a property the schema does not list resolves to null
/// every time, and so does reading *any* property from an agent that declares no
/// schema at all: without one there is no structured output to address, only the
/// raw completion.
///
/// Two deliberate narrowings, each one a false positive avoided:
///
/// - **Only a `tool_call`'s `args`.** An agent's own prompt has no schema to
///   check a mention against, and a vaguer answer is not a broken call.
/// - **Only a binding that went through the envelope.** One that did not is
///   already reported by [`binding_failures`], and the author has to fix that
///   first; saying it twice in different words reads as two problems.
///
/// Only the first path segment is compared. A schema declares its top-level
/// properties; how deep a value nests below one is the model's business.
fn agent_schema_failures(graph: &WorkflowGraph) -> Vec<String> {
    let mut failures = Vec::new();
    for node in &graph.nodes {
        if node.kind != NodeKind::ToolCall {
            continue;
        }
        let Some(args) = node.config.get("args") else {
            continue;
        };
        for (location, expr) in collect_expressions(args) {
            let Some(binding) = parse_node_binding(&expr) else {
                continue;
            };
            if !binding.through_envelope {
                continue;
            }
            let Some(target) = bindings::node_of(graph, &binding.node_id) else {
                continue;
            };
            if target.kind != NodeKind::Agent {
                continue;
            }
            let field = binding
                .field_path
                .split('.')
                .next()
                .unwrap_or(&binding.field_path);
            let Some(properties) = target
                .config
                .get("output_parser")
                .and_then(|parser| parser.get("schema"))
                .filter(|schema| !schema.is_null())
                .and_then(|schema| schema.get("properties"))
                .and_then(|properties| properties.as_object())
            else {
                // Without a declared schema the agent's runtime response is
                // host-defined. The field may exist, so it is unverifiable
                // rather than guaranteed invalid.
                continue;
            };
            if properties.contains_key(field) {
                continue;
            }
            failures.push(format!(
                "node '{}': `{location}` (`{expr}`) reads `{field}` from agent node \
                 `{target_id}`, whose `output_parser.schema` does not declare it — the agent \
                 has no addressable `{field}`, so this resolves to null at run time and the \
                 call is made with nothing in it. Fix: declare `{field}` in node \
                 `{target_id}`'s `output_parser.schema`, or bind to a property it already \
                 declares.",
                node.id,
                target_id = binding.node_id,
            ));
        }
    }
    failures
}

#[cfg(test)]
#[path = "gates_tests.rs"]
mod tests;
