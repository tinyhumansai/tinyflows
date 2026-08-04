//! Declared workflow inputs: the typed, caller-supplied parameters of a run.
//!
//! A [`WorkflowGraph`](crate::model::WorkflowGraph) declares zero or more
//! [`WorkflowInput`]s. They are the workflow's *public signature* — what a
//! caller must (or may) provide to run it — and are deliberately independent of
//! the trigger kind, so a manually run graph, a scheduled one, and one invoked
//! as a `sub_workflow` all expose the same parameters.
//!
//! Declared inputs are distinct from the free-form trigger payload:
//!
//! | | trigger payload (`run.trigger`) | declared inputs (`inputs`) |
//! |---|---|---|
//! | shape | whatever fired the run (webhook body, chat message, …) | named, typed, validated |
//! | discoverable | no | yes — from the graph |
//! | addressed as | `=run.trigger.<path>` | `=inputs.<name>` |
//!
//! Supplied values are checked by [`resolve_inputs`] **before** a run starts, so
//! a missing required parameter fails loudly instead of surfacing as a `null`
//! deep inside some node's configuration.
//!
//! Inputs are not a secret channel. Credentials reach a workflow through the
//! opaque connection reference the host resolves (see [`crate::caps`]); an input
//! is ordinary run data and is journalled and observable like any other.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

/// The declared type of a [`WorkflowInput`].
///
/// Type checking is deliberately shallow: it catches a caller passing a string
/// where a number was declared, and nothing more. Anything with real structure
/// is declared [`Json`](InputType::Json) and validated by the workflow itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputType {
    /// A JSON string. The default when `type` is omitted.
    #[default]
    String,
    /// A JSON number (integer or float).
    Number,
    /// A JSON boolean.
    Boolean,
    /// Any JSON value — object, array, or scalar. Accepts everything.
    Json,
}

impl InputType {
    /// The `snake_case` wire name of this type, as it appears in JSON.
    ///
    /// ```
    /// use tinyflows::model::InputType;
    ///
    /// assert_eq!(InputType::Boolean.as_str(), "boolean");
    /// ```
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Json => "json",
        }
    }

    /// Whether `value` satisfies this declared type.
    ///
    /// [`Json`](InputType::Json) accepts any value, including `null`. The scalar
    /// types accept only their own JSON type — no coercion, so `"3"` is not a
    /// [`Number`](InputType::Number). Hosts that read values from a
    /// string-shaped surface (a CLI flag, a text prompt) are expected to coerce
    /// before calling; see the `medulla` CLI's `--set` handling for a reference.
    ///
    /// ```
    /// use tinyflows::model::InputType;
    /// use serde_json::json;
    ///
    /// assert!(InputType::Number.accepts(&json!(3)));
    /// assert!(!InputType::Number.accepts(&json!("3")));
    /// assert!(InputType::Json.accepts(&json!({"any": "shape"})));
    /// ```
    #[must_use]
    pub fn accepts(self, value: &Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Number => value.is_number(),
            Self::Boolean => value.is_boolean(),
            Self::Json => true,
        }
    }
}

/// One declared parameter of a workflow.
///
/// ```
/// use tinyflows::model::{InputType, WorkflowInput};
///
/// let declared: WorkflowInput = serde_json::from_str(
///     r#"{"name":"repo","type":"string","required":true,"description":"Repo to review"}"#,
/// )
/// .unwrap();
/// assert_eq!(declared.name, "repo");
/// assert_eq!(declared.ty, InputType::String);
/// assert!(declared.required);
///
/// // `type` defaults to `string`, and an input is optional unless declared otherwise.
/// let minimal: WorkflowInput = serde_json::from_str(r#"{"name":"note"}"#).unwrap();
/// assert_eq!(minimal.ty, InputType::String);
/// assert!(!minimal.required);
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowInput {
    /// The parameter's name, and the key it is addressed by in expressions
    /// (`=inputs.<name>`). Must be a plain identifier — see
    /// [`is_valid_input_name`].
    pub name: String,
    /// The declared JSON type. Defaults to [`InputType::String`].
    #[serde(rename = "type", default)]
    pub ty: InputType,
    /// Human-readable explanation, shown by authoring and run surfaces (the
    /// hint line of a prompt, the description of a generated tool parameter).
    #[serde(default)]
    pub description: Option<String>,
    /// Whether a caller must supply this input. A required input with no
    /// supplied value fails the run before it starts.
    #[serde(default)]
    pub required: bool,
    /// Value used when the caller supplies none. Mutually exclusive with
    /// `required` — a default means there is always a value.
    #[serde(default)]
    pub default: Option<Value>,
}

impl WorkflowInput {
    /// A minimal optional input of the given name and type.
    #[must_use]
    pub fn new(name: impl Into<String>, ty: InputType) -> Self {
        Self {
            name: name.into(),
            ty,
            description: None,
            required: false,
            default: None,
        }
    }

    /// Marks this input as required, so a run without it fails to start.
    #[must_use]
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Sets the value used when the caller supplies none.
    #[must_use]
    pub fn with_default(mut self, default: Value) -> Self {
        self.default = Some(default);
        self
    }

    /// Sets the human-readable description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Whether `name` is usable as an input name.
///
/// Names must match `[A-Za-z_][A-Za-z0-9_]*` so that `=inputs.<name>` resolves
/// through the fast dotted-path walk in [`crate::expr`] without jq quoting. A
/// name that needed escaping would work in one expression form and silently
/// misbehave in the other, so it is rejected at validation time instead.
///
/// ```
/// use tinyflows::model::is_valid_input_name;
///
/// assert!(is_valid_input_name("repo_url"));
/// assert!(!is_valid_input_name("repo-url"));   // would need jq quoting
/// assert!(!is_valid_input_name("2fa"));        // leading digit
/// assert!(!is_valid_input_name(""));
/// ```
#[must_use]
pub fn is_valid_input_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Why a set of supplied input values was rejected.
///
/// Every variant names the offending input, so a host can attach the failure to
/// the right form field rather than showing a whole-run error.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InputError {
    /// A required input was not supplied and has no default.
    #[error("workflow input {0:?} is required but was not supplied")]
    Missing(String),

    /// A supplied value's JSON type does not match the declared type.
    #[error("workflow input {name:?} expects type {expected} but received {found}")]
    TypeMismatch {
        /// The declared input's name.
        name: String,
        /// The declared type's wire name.
        expected: &'static str,
        /// The supplied value's JSON type name.
        found: &'static str,
    },

    /// A value was supplied under a name the workflow does not declare.
    ///
    /// Rejected rather than ignored: a silently dropped value looks identical to
    /// a workflow that read it and did nothing. Callers with genuinely
    /// free-form data should use the trigger payload instead.
    #[error("workflow does not declare an input named {0:?}")]
    Unknown(String),
}

impl InputError {
    /// A stable, machine-readable code for this variant, for hosts that surface
    /// structured errors rather than prose.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing(_) => "input_missing",
            Self::TypeMismatch { .. } => "input_type_mismatch",
            Self::Unknown(_) => "input_unknown",
        }
    }

    /// The name of the input this error is about.
    #[must_use]
    pub fn input_name(&self) -> &str {
        match self {
            Self::Missing(name) | Self::Unknown(name) => name,
            Self::TypeMismatch { name, .. } => name,
        }
    }
}

/// The JSON type name of `value`, for [`InputError::TypeMismatch`] reporting.
fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Validates `supplied` against `declared` and returns the resolved values.
///
/// The returned map has **exactly one entry per declared input** — callers can
/// index it without checking for absence. Resolution per input:
///
/// - supplied → type-checked, then used as-is;
/// - absent with a `default` → the default;
/// - absent and `required` → [`InputError::Missing`];
/// - absent, optional, no default → [`Value::Null`].
///
/// A supplied key with no matching declaration is [`InputError::Unknown`].
///
/// # Errors
/// Returns the first problem found, scanning declarations in order and then
/// checking for undeclared keys.
///
/// ```
/// use tinyflows::model::{resolve_inputs, InputError, InputType, WorkflowInput};
/// use serde_json::{json, Map};
///
/// let declared = vec![
///     WorkflowInput::new("repo", InputType::String).required(),
///     WorkflowInput::new("depth", InputType::Number).with_default(json!(3)),
///     WorkflowInput::new("note", InputType::String),
/// ];
///
/// let mut supplied = Map::new();
/// supplied.insert("repo".into(), json!("acme/api"));
///
/// let resolved = resolve_inputs(&declared, &supplied).unwrap();
/// assert_eq!(resolved["repo"], json!("acme/api"));
/// assert_eq!(resolved["depth"], json!(3));      // default applied
/// assert_eq!(resolved["note"], json!(null));    // optional, no default
///
/// // A required input with no value fails before the run starts.
/// let err = resolve_inputs(&declared, &Map::new()).unwrap_err();
/// assert_eq!(err, InputError::Missing("repo".into()));
/// ```
pub fn resolve_inputs(
    declared: &[WorkflowInput],
    supplied: &Map<String, Value>,
) -> std::result::Result<Map<String, Value>, InputError> {
    let mut resolved = Map::new();

    for input in declared {
        let value = match supplied.get(&input.name) {
            Some(value) => {
                if !input.ty.accepts(value) {
                    return Err(InputError::TypeMismatch {
                        name: input.name.clone(),
                        expected: input.ty.as_str(),
                        found: json_type_name(value),
                    });
                }
                value.clone()
            }
            None => match &input.default {
                Some(default) => default.clone(),
                None if input.required => return Err(InputError::Missing(input.name.clone())),
                None => Value::Null,
            },
        };
        resolved.insert(input.name.clone(), value);
    }

    // Undeclared keys are checked last so a caller fixing a real declaration
    // problem is not first told about a typo in an unrelated key.
    for name in supplied.keys() {
        if !resolved.contains_key(name) {
            return Err(InputError::Unknown(name.clone()));
        }
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn supplied(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn input_type_defaults_to_string_and_round_trips() {
        let input: WorkflowInput = serde_json::from_str(r#"{"name":"note"}"#).unwrap();
        assert_eq!(input.ty, InputType::String);
        assert!(!input.required);
        assert_eq!(input.default, None);
        assert_eq!(input.description, None);

        let json = serde_json::to_value(&input).unwrap();
        assert_eq!(json["type"], "string");
        let back: WorkflowInput = serde_json::from_value(json).unwrap();
        assert_eq!(back, input);
    }

    #[test]
    fn input_type_accepts_matching_json_only() {
        assert!(InputType::String.accepts(&json!("x")));
        assert!(!InputType::String.accepts(&json!(1)));
        assert!(InputType::Number.accepts(&json!(1.5)));
        assert!(!InputType::Number.accepts(&json!("1.5")));
        assert!(InputType::Boolean.accepts(&json!(true)));
        assert!(!InputType::Boolean.accepts(&json!("true")));
        for value in [json!(null), json!([1]), json!({"a": 1}), json!("s")] {
            assert!(InputType::Json.accepts(&value), "json rejected {value}");
        }
    }

    #[test]
    fn valid_input_names() {
        for name in ["a", "_", "repo", "repo_url", "_x1", "A9"] {
            assert!(is_valid_input_name(name), "{name} should be valid");
        }
        for name in ["", "1a", "repo-url", "repo.url", "repo url", "café"] {
            assert!(!is_valid_input_name(name), "{name} should be invalid");
        }
    }

    #[test]
    fn resolves_supplied_default_and_null() {
        let declared = vec![
            WorkflowInput::new("repo", InputType::String).required(),
            WorkflowInput::new("depth", InputType::Number).with_default(json!(3)),
            WorkflowInput::new("note", InputType::String),
        ];
        let resolved =
            resolve_inputs(&declared, &supplied(&[("repo", json!("acme/api"))])).unwrap();

        assert_eq!(resolved.len(), 3, "one entry per declared input");
        assert_eq!(resolved["repo"], json!("acme/api"));
        assert_eq!(resolved["depth"], json!(3));
        assert_eq!(resolved["note"], json!(null));
    }

    #[test]
    fn supplied_value_overrides_default() {
        let declared = vec![WorkflowInput::new("depth", InputType::Number).with_default(json!(3))];
        let resolved = resolve_inputs(&declared, &supplied(&[("depth", json!(9))])).unwrap();
        assert_eq!(resolved["depth"], json!(9));
    }

    #[test]
    fn missing_required_input_is_rejected() {
        let declared = vec![WorkflowInput::new("repo", InputType::String).required()];
        let err = resolve_inputs(&declared, &Map::new()).unwrap_err();
        assert_eq!(err, InputError::Missing("repo".into()));
        assert_eq!(err.code(), "input_missing");
        assert_eq!(err.input_name(), "repo");
        assert_eq!(
            err.to_string(),
            "workflow input \"repo\" is required but was not supplied"
        );
    }

    #[test]
    fn type_mismatch_is_rejected_without_coercion() {
        let declared = vec![WorkflowInput::new("depth", InputType::Number)];
        let err = resolve_inputs(&declared, &supplied(&[("depth", json!("3"))])).unwrap_err();
        assert_eq!(
            err,
            InputError::TypeMismatch {
                name: "depth".into(),
                expected: "number",
                found: "string",
            }
        );
        assert_eq!(err.code(), "input_type_mismatch");
        assert_eq!(
            err.to_string(),
            "workflow input \"depth\" expects type number but received string"
        );
    }

    #[test]
    fn explicit_null_fails_a_scalar_type_but_passes_json() {
        let scalar = vec![WorkflowInput::new("depth", InputType::Number)];
        assert!(resolve_inputs(&scalar, &supplied(&[("depth", json!(null))])).is_err());

        let any = vec![WorkflowInput::new("payload", InputType::Json)];
        let resolved = resolve_inputs(&any, &supplied(&[("payload", json!(null))])).unwrap();
        assert_eq!(resolved["payload"], json!(null));
    }

    #[test]
    fn undeclared_key_is_rejected() {
        let declared = vec![WorkflowInput::new("repo", InputType::String)];
        let err = resolve_inputs(
            &declared,
            &supplied(&[("repo", json!("a")), ("reop", json!("typo"))]),
        )
        .unwrap_err();
        assert_eq!(err, InputError::Unknown("reop".into()));
        assert_eq!(err.code(), "input_unknown");
    }

    #[test]
    fn declaration_errors_are_reported_before_undeclared_keys() {
        // A caller who both forgot a required input and mistyped another key
        // hears about the required one first — it is the actionable failure.
        let declared = vec![WorkflowInput::new("repo", InputType::String).required()];
        let err = resolve_inputs(&declared, &supplied(&[("reop", json!("typo"))])).unwrap_err();
        assert_eq!(err, InputError::Missing("repo".into()));
    }

    #[test]
    fn no_declarations_accepts_nothing_and_yields_empty() {
        assert!(resolve_inputs(&[], &Map::new()).unwrap().is_empty());
        assert_eq!(
            resolve_inputs(&[], &supplied(&[("x", json!(1))])).unwrap_err(),
            InputError::Unknown("x".into())
        );
    }

    #[test]
    fn builders_compose() {
        let input = WorkflowInput::new("depth", InputType::Number)
            .with_default(json!(3))
            .with_description("How deep to recurse");
        assert_eq!(input.default, Some(json!(3)));
        assert_eq!(input.description.as_deref(), Some("How deep to recurse"));
        assert!(!input.required);
        assert!(
            WorkflowInput::new("repo", InputType::String)
                .required()
                .required
        );
    }
}
