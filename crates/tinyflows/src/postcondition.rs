//! Node postconditions: a mechanical check on a node's output before it
//! counts as a success.
//!
//! The engine advances the moment an executor returns `Ok`; nothing asks
//! whether the output is actually enough to hand downstream. A capped or
//! refused agent turn that still produced *some* prose, or a tool whose reply
//! is missing the field the next node binds to, flows on as if it were a
//! finished answer, and the run reads as healthy.
//!
//! A postcondition is the author saying "this output is not good enough to
//! advance on unless it has this shape". It is declared in a node's free-form
//! config, the same way `retry` and `on_error` are:
//!
//! ```json
//! { "postcondition": { "require": "field_present", "field": "json.items" } }
//! ```
//!
//! The engine checks it against **every item** the node emits, after the
//! executor returns and before the attempt counts as a success. An output that
//! fails it is a failed attempt like any other: it is retried under
//! `retry.max_attempts`, and once attempts run out `on_error` decides what
//! happens — by default the run stops, so nothing downstream sees the
//! insufficient output. An output carrying a [`NodeControl`] (a pause, a
//! re-entry, a fan-out) is not the node's settled answer and is not checked.
//!
//! Deliberately narrow: three predicates, no model call, no network, no state.
//! See [`REQUIREMENTS`].
//!
//! # A gate that cannot be evaluated fails the node
//!
//! [`Postcondition::validate`] runs at author time (through
//! [`crate::validate::validate_all`]), so an unrecognised `require` reaching
//! the engine can only mean a graph saved by a build whose validator knows
//! predicates this one does not. Advancing anyway would not skip a
//! measurement, it would silently delete the check — so an unrecognised
//! `require` **fails** the node, with a message naming it. A `field_present`
//! with no `field` fails closed for the same reason: it has nothing left to
//! check, and passing it would switch the gate off.
//!
//! # The declaration is read raw
//!
//! Unlike most config, the postcondition is never `=`-expression resolved: it
//! describes the node's output, which does not exist when config is resolved.
//! `field` is therefore a literal dotted path into an emitted item's `json`,
//! and an `=`-expression there is an authoring error [`Postcondition::validate`]
//! refuses.
//!
//! [`NodeControl`]: crate::nodes::NodeControl

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::data::Item;
use crate::error::EngineError;
use crate::model::Node;
use crate::nodes::NodeOutput;

/// The predicates a [`Postcondition::require`] may name, in the order an
/// authoring error lists them.
///
/// - `non_empty` — the item's `text` is a string with something other than
///   whitespace in it. For a capability node's envelope that is the model's or
///   tool's prose.
/// - `field_present` — the dotted [`Postcondition::field`] path resolves, via
///   object keys only, to a present, non-null value in the item.
/// - `non_empty_list` — the target is an array with at least one element. The
///   target is the `field` path when one is given; otherwise the envelope's
///   structured `json` payload, or the whole item when it has no `json` key.
pub const REQUIREMENTS: [&str; 3] = ["non_empty", "field_present", "non_empty_list"];

/// A node's declared postcondition: the `postcondition` key of its config.
///
/// `require` is a plain string rather than an enum on purpose: a graph saved
/// by a newer build must still *load* here, so that the engine can refuse to
/// advance past a predicate it cannot evaluate instead of failing to parse the
/// whole graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Postcondition {
    /// Which predicate to check — one of [`REQUIREMENTS`].
    #[serde(default)]
    pub require: String,
    /// A dotted path into an emitted item (e.g. `json.items`). Required for
    /// `field_present`, optional for `non_empty_list`, unused by `non_empty`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

impl Postcondition {
    /// Reads the postcondition a node's `config` declares.
    ///
    /// `None` when the node declares none. `Some(Err(reason))` when the
    /// `postcondition` key is present but is not a postcondition object — a
    /// declared gate that cannot be read is an error, never an absence.
    #[must_use]
    pub fn from_config(config: &Value) -> Option<Result<Self, String>> {
        let declared = config.get("postcondition")?;
        // Checked before decoding: serde's derive also accepts a sequence for
        // a struct, which would read `["non_empty"]` as a real declaration.
        if !declared.is_object() {
            return Some(Err(format!(
                "the node's `postcondition` is malformed: expected an object like \
                 {{\"require\": \"non_empty\"}}, got {declared}"
            )));
        }
        Some(
            serde_json::from_value(declared.clone())
                .map_err(|err| format!("the node's `postcondition` is malformed: {err}")),
        )
    }

    /// Checks one output value — an emitted item's `json`.
    ///
    /// # Errors
    ///
    /// A plain-English sentence naming what the output is missing, suitable as
    /// the failed attempt's error message.
    pub fn check(&self, output: &Value) -> Result<(), String> {
        let field = self.field.as_deref().filter(|f| !f.is_empty());
        match self.require.as_str() {
            "non_empty" => {
                let text = output.get("text").and_then(Value::as_str).unwrap_or("");
                if text.trim().is_empty() {
                    Err("the output was empty — nothing was produced to advance on.".to_string())
                } else {
                    Ok(())
                }
            }
            "field_present" => {
                let Some(path) = field else {
                    return Err("the postcondition requires `field_present` but names no \
                                `field` to check — refusing to advance on an unverifiable gate."
                        .to_string());
                };
                match resolve_path(output, path) {
                    Some(value) if !value.is_null() => Ok(()),
                    _ => Err(format!(
                        "the output is missing `{path}` — the expected field never landed."
                    )),
                }
            }
            "non_empty_list" => {
                let target = match field {
                    Some(path) => resolve_path(output, path),
                    // The envelope wrapper is always an object, so "the
                    // output" means its structured payload; a value with no
                    // `json` key is checked as a whole.
                    None => output.get("json").or(Some(output)),
                };
                let described =
                    field.map_or_else(|| "the output".to_string(), |p| format!("`{p}`"));
                match target {
                    Some(Value::Array(items)) if !items.is_empty() => Ok(()),
                    Some(Value::Array(_)) => Err(format!(
                        "{described} is an empty list — nothing came back to advance on."
                    )),
                    Some(Value::Null) | None => Err(format!(
                        "{described} is missing — nothing came back to advance on."
                    )),
                    Some(_) => Err(format!(
                        "{described} is not a list — the shape does not match."
                    )),
                }
            }
            other => Err(format!(
                "the postcondition requires `{other}`, which this build does not know how to \
                 check — refusing to advance rather than silently pass an unevaluated gate."
            )),
        }
    }

    /// Checks every item a node emitted. A node that emitted none fails: no
    /// output has the declared shape.
    ///
    /// # Errors
    ///
    /// The first failing item's gap, prefixed with its index when the node
    /// emitted more than one.
    pub fn check_items(&self, items: &[Item]) -> Result<(), String> {
        if items.is_empty() {
            return Err(format!(
                "the node emitted no items, so nothing can satisfy `{}`.",
                self.require
            ));
        }
        for (index, item) in items.iter().enumerate() {
            self.check(&item.json).map_err(|gap| {
                if items.len() == 1 {
                    gap
                } else {
                    format!("item {index}: {gap}")
                }
            })?;
        }
        Ok(())
    }

    /// The author-time check: a known predicate, a `field` where one is
    /// required, and a `field` that is a literal dotted path.
    ///
    /// # Errors
    ///
    /// Why the declaration can never be evaluated as written.
    pub fn validate(&self) -> Result<(), String> {
        if !REQUIREMENTS.contains(&self.require.as_str()) {
            return Err(format!(
                "unknown `postcondition.require` `{}` — use one of {}",
                self.require,
                REQUIREMENTS.join(", ")
            ));
        }
        let field = self.field.as_deref().map(str::trim);
        if self.require == "field_present" && field.is_none_or(str::is_empty) {
            return Err(
                "`postcondition.require = \"field_present\"` needs a `field` naming what it must \
                 find"
                    .to_string(),
            );
        }
        let malformed = field
            .filter(|f| !f.is_empty())
            .filter(|f| f.starts_with('=') || f.split('.').any(str::is_empty));
        if let Some(field) = malformed {
            return Err(format!(
                "`postcondition.field` `{field}` must be a literal dotted path into the node's \
                 output (e.g. `json.items`); it is never expression-resolved"
            ));
        }
        Ok(())
    }
}

/// The engine's side of the gate: checks the postcondition `node` declares
/// against an executor's successful `output`.
///
/// An output carrying a control request is not the node's settled answer and
/// passes unchecked. A declaration that cannot be read fails the attempt.
pub(crate) fn enforce(node: &Node, output: &NodeOutput) -> Result<(), EngineError> {
    if output.control.is_some() {
        return Ok(());
    }
    let Some(declared) = Postcondition::from_config(&node.config) else {
        return Ok(());
    };
    declared
        .and_then(|postcondition| postcondition.check_items(&output.items))
        .map_err(|gap| {
            tracing::warn!(node = %node.id, %gap, "node output failed its postcondition");
            EngineError::Capability(format!(
                "node '{}' failed its postcondition: {gap}",
                node.id
            ))
        })
}

/// Resolves a dot-separated path (`"a.b.c"`) through nested JSON objects.
/// Every hop is an object-key lookup; arrays are not indexed.
fn resolve_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(value, |acc, key| acc.as_object()?.get(key))
}

#[cfg(test)]
#[path = "postcondition_tests.rs"]
mod tests;
