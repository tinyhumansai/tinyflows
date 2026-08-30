//! Reading the `=`-expressions out of a graph, and deciding which of them are
//! already known to be wrong.
//!
//! Everything here is a pure function of the graph. That is what makes it a
//! *gate* rather than a diagnostic: it runs before a write, costs nothing, and
//! can refuse an edit outright.

use crate::model::{NodeKind, WorkflowGraph};
use serde_json::Value;

/// The envelope's own accessors.
///
/// Reading `.item.text` off an agent is not a mistake — it is how you get the
/// completion text, and `.item.raw` is the untouched response. A binding whose
/// first path segment is one of these is addressing the envelope itself, not a
/// field the author forgot to reach through it.
pub const ENVELOPE_FIELDS: [&str; 3] = ["json", "text", "raw"];

/// Whether `field_path` addresses the envelope itself rather than something
/// inside it.
///
/// The whole path must be an accessor: `text` is the envelope, while
/// `text.value` tries to descend into a string and is not a valid accessor.
#[must_use]
pub fn reads_the_envelope_itself(field_path: &str) -> bool {
    ENVELOPE_FIELDS.contains(&field_path)
}

/// Node kinds whose output is wrapped in the engine's `{json, text, raw}`
/// envelope.
///
/// The distinction the envelope creates is the single most common way a graph
/// compiles, validates, dry-runs green, and then does nothing: `=nodes.x.item.f`
/// reads a field that lives at `=nodes.x.item.json.f`, so it resolves to null
/// and the step runs with an empty value.
const ENVELOPING_KINDS: [NodeKind; 3] =
    [NodeKind::Agent, NodeKind::ToolCall, NodeKind::HttpRequest];

/// jq keywords that read as syntax rather than as prose.
///
/// Used by [`reads_as_prose`] so a genuine jq program — `if`/`then`/`else`,
/// `reduce`, a `def` — is never mistaken for natural language.
const JQ_KEYWORDS: &[&str] = &[
    "and", "or", "not", "if", "then", "elif", "else", "end", "as", "def", "reduce", "foreach",
    "try", "catch", "import", "include", "label",
];

/// Every `=`-expression in `value`, paired with its dotted location.
///
/// Walks objects and arrays, so a binding nested inside a tool call's `args`
/// is found and can be named precisely in an error an author has to act on.
pub fn collect_expressions(value: &Value) -> Vec<(String, String)> {
    fn walk(value: &Value, location: &str, out: &mut Vec<(String, String)>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let path = if location.is_empty() {
                        key.clone()
                    } else {
                        format!("{location}.{key}")
                    };
                    walk(child, &path, out);
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    let path = if location.is_empty() {
                        index.to_string()
                    } else {
                        format!("{location}.{index}")
                    };
                    walk(child, &path, out);
                }
            }
            Value::String(text) if crate::expr::is_expression(text) => {
                out.push((location.to_string(), text.clone()));
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(value, "", &mut out);
    out
}

/// One node-output binding, taken apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeBinding {
    /// The node whose output is being read.
    pub node_id: String,
    /// Whether the expression went through the `{json, text, raw}` envelope.
    pub through_envelope: bool,
    /// The remaining dotted path, whole — `data.messages`, not `data`.
    pub field_path: String,
}

/// Parse `=nodes.<id>.item[.json].<path>`, if the expression is one.
///
/// Deliberately narrow. An expression this does not match is not reported on at
/// all, because a gate that guessed at arbitrary jq would refuse graphs that are
/// fine — and a false refusal costs an author their edit.
///
/// Hand-written rather than a regular expression
/// (`^=nodes\.([A-Za-z_]\w*)\.item(?:\.(json))?\.([A-Za-z_][\w.]*)`), because a
/// regex engine is a large dependency for one anchored pattern in a crate that
/// otherwise needs none.
pub fn parse_node_binding(expr: &str) -> Option<NodeBinding> {
    let rest = expr.strip_prefix("=nodes.")?;
    let (node_id, rest) = take_identifier(rest)?;
    let rest = rest.strip_prefix(".item")?;

    // `.json` is optional, and only counts when it is a whole path segment: a
    // node field actually named `jsonish` must not be mistaken for the envelope.
    let (through_envelope, rest) = match rest.strip_prefix(".json") {
        Some(after) if after.starts_with('.') => (true, after),
        _ => (false, rest),
    };

    let rest = rest.strip_prefix('.')?;
    let (field_path, _) = take_field_path(rest)?;
    let through_envelope = through_envelope || matches!(field_path.as_str(), "text" | "raw");
    Some(NodeBinding {
        node_id: node_id.to_string(),
        through_envelope,
        field_path,
    })
}

/// The leading `[A-Za-z_][A-Za-z0-9_]*`, and what follows it.
fn take_identifier(input: &str) -> Option<(&str, &str)> {
    let mut end = 0;
    for (index, ch) in input.char_indices() {
        let ok = if index == 0 {
            ch.is_ascii_alphabetic() || ch == '_'
        } else {
            ch.is_ascii_alphanumeric() || ch == '_'
        };
        if !ok {
            break;
        }
        end = index + ch.len_utf8();
    }
    (end > 0).then(|| input.split_at(end))
}

/// The leading `[A-Za-z_][A-Za-z0-9_.]*`, with trailing dots trimmed.
///
/// Trailing dots are trimmed rather than refused because the pattern this
/// replaces did the same: `=nodes.a.item.field.` names `field`, and the dot is
/// the author's typo, not a different binding.
fn take_field_path(input: &str) -> Option<(String, &str)> {
    let mut end = 0;
    for (index, ch) in input.char_indices() {
        let ok = if index == 0 {
            ch.is_ascii_alphabetic() || ch == '_'
        } else {
            ch.is_ascii_alphanumeric() || ch == '_' || ch == '.'
        };
        if !ok {
            break;
        }
        end = index + ch.len_utf8();
    }
    if end == 0 {
        return None;
    }
    let (matched, rest) = input.split_at(end);
    let trimmed = matched.trim_end_matches('.');
    (!trimmed.is_empty()).then(|| (trimmed.to_string(), rest))
}

/// Whether an expression body reads as prose rather than as a jq program.
///
/// The failure this catches is specific and common: an author writes
/// `"=You are given an issue: .item. Summarise it"` in a node's `prompt`,
/// believing `=` interpolates. It does not — jq has no rule for two bare words
/// in a row — so the whole expression resolves to null and the step runs with an
/// empty prompt. Nothing else notices: it parses, it validates, and it produces
/// a plausible-looking run.
///
/// Best-effort by construction, and biased toward *not* firing: two consecutive
/// bare alphabetic words outside a string literal, none of them jq keywords.
pub fn reads_as_prose(expr_body: &str) -> bool {
    // String literals are stripped first: `"two words"` inside a real jq
    // program is data, not prose, and would otherwise trip the scan.
    let mut stripped = String::with_capacity(expr_body.len());
    let mut in_string = false;
    let mut chars = expr_body.chars();
    while let Some(c) = chars.next() {
        // An escaped character inside a literal — consume both, so `\"` does
        // not read as the end of the string.
        if in_string && c == '\\' {
            chars.next();
            continue;
        }
        if c == '"' {
            in_string = !in_string;
            continue;
        }
        if !in_string {
            stripped.push(c);
        }
    }

    let mut consecutive = 0u32;
    for token in stripped.split_whitespace() {
        let core = token.trim_matches(|c: char| !c.is_ascii_alphabetic());
        let bare = !core.is_empty()
            && core.chars().all(|c| c.is_ascii_alphabetic())
            && !token.starts_with('.')
            && !token.contains('.')
            && !JQ_KEYWORDS.contains(&core.to_ascii_lowercase().as_str());
        if bare {
            consecutive += 1;
            if consecutive >= 2 {
                return true;
            }
        } else {
            consecutive = 0;
        }
    }
    false
}

/// Whether `kind` wraps its output in the `{json, text, raw}` envelope.
pub fn wraps_output(kind: &NodeKind) -> bool {
    ENVELOPING_KINDS.contains(kind)
}

/// A node kind named the way an error message should name it.
pub fn kind_article(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::Agent => "an agent",
        NodeKind::ToolCall => "a tool_call",
        NodeKind::HttpRequest => "an http_request",
        _ => "a node",
    }
}

/// The node with `id`, if the graph has one.
pub fn node_of<'a>(graph: &'a WorkflowGraph, id: &str) -> Option<&'a crate::model::Node> {
    graph.nodes.iter().find(|node| node.id == id)
}
