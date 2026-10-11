//! Mock child input values and path-level provenance of unavailable runtime data.

use serde_json::{Map, Value, json};

use crate::model::{InputType, WorkflowGraph};

#[derive(Clone, Default)]
pub(super) struct MockInputs {
    pub(super) values: Map<String, Value>,
    pub(super) unknown: Vec<Vec<String>>,
}

impl MockInputs {
    /// Root declared inputs are supplied by a real caller, outside this mock run.
    pub(super) fn root(graph: &WorkflowGraph) -> Self {
        Self {
            values: graph
                .inputs
                .iter()
                .filter(|input| input.required)
                .map(|input| (input.name.clone(), placeholder(input.ty)))
                .collect(),
            unknown: graph
                .inputs
                .iter()
                .map(|input| vec![input.name.clone()])
                .collect(),
        }
    }

    /// Preserve literals and known parent inputs; retain uncertainty across child mappings.
    pub(super) fn child(&self, graph: &WorkflowGraph, config: &Value) -> Self {
        let mut child = Self::default();
        let Some(supplied) = config.get("inputs").and_then(Value::as_object) else {
            return child;
        };
        for input in &graph.inputs {
            let Some(raw) = supplied.get(&input.name) else {
                continue;
            };
            let mut value = crate::expr::resolve(raw, &json!({"inputs": self.values}));
            self.collect_unknown(raw, std::slice::from_ref(&input.name), &mut child.unknown);
            if raw.as_str().is_some_and(crate::expr::is_expression) && !input.ty.accepts(&value) {
                value = placeholder(input.ty);
            }
            child.values.insert(input.name.clone(), value);
        }
        child
    }

    /// A read intersecting an opaque input path cannot prove a field is absent.
    pub(super) fn is_unverifiable(&self, expression: &str) -> bool {
        input_path(expression).is_some_and(|path| {
            self.unknown
                .iter()
                .any(|unknown| path.starts_with(unknown) || unknown.starts_with(&path))
        })
    }

    /// Transfer only the unavailable leaves when JSON mixes literal and dynamic fields.
    fn collect_unknown(&self, raw: &Value, destination: &[String], out: &mut Vec<Vec<String>>) {
        match raw {
            Value::Object(values) => {
                for (key, value) in values {
                    let mut path = destination.to_vec();
                    path.push(key.clone());
                    self.collect_unknown(value, &path, out);
                }
            }
            Value::Array(values) => {
                for (index, value) in values.iter().enumerate() {
                    let mut path = destination.to_vec();
                    path.push(index.to_string());
                    self.collect_unknown(value, &path, out);
                }
            }
            Value::String(expression) if crate::expr::is_expression(expression) => {
                if scope_reads(expression)
                    .iter()
                    .any(|(scope, _)| scope != "inputs")
                {
                    out.push(destination.to_vec());
                    return;
                }
                if let Some(source) = input_path(expression) {
                    if source.is_empty() && !self.unknown.is_empty() {
                        out.push(destination.to_vec());
                        return;
                    }
                    for unknown in &self.unknown {
                        if source.starts_with(unknown) {
                            out.push(destination.to_vec());
                        } else if let Some(suffix) = unknown.strip_prefix(source.as_slice()) {
                            let mut path = destination.to_vec();
                            path.extend_from_slice(suffix);
                            out.push(path);
                        }
                    }
                } else if scope_reads(expression).iter().any(|(scope, path)| {
                    scope != "inputs"
                        || self
                            .unknown
                            .iter()
                            .any(|unknown| path.starts_with(unknown) || unknown.starts_with(path))
                }) {
                    // Only expressions reading unavailable data are opaque;
                    // constant jq mappings retain their concrete values.
                    out.push(destination.to_vec());
                }
            }
            _ => {}
        }
    }
}

/// Recognize input paths in shorthand, bracket notation, and jq field-access pipes.
/// Other jq operators conservatively address the entire input scope.
fn input_path(expression: &str) -> Option<Vec<String>> {
    let body = expression.strip_prefix('=')?.trim();
    let body = body.strip_prefix('.').unwrap_or(body);
    let mut rest = body.strip_prefix("inputs")?;
    if !rest.is_empty() && !rest.starts_with(['.', '[', ' ', '|']) {
        return None;
    }
    let mut path = Vec::new();
    while !rest.trim().is_empty() {
        rest = rest.trim_start();
        if let Some(tail) = rest.strip_prefix('|') {
            rest = tail.trim_start();
            if !rest.starts_with(['.', '[']) {
                return Some(Vec::new());
            }
        }
        if let Some(tail) = rest.strip_prefix('.') {
            let end = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                .unwrap_or(tail.len());
            if end == 0 {
                return Some(Vec::new());
            }
            path.push(tail[..end].to_string());
            rest = &tail[end..];
        } else if let Some(tail) = rest.strip_prefix('[') {
            let Some(end) = tail.find(']') else {
                return Some(Vec::new());
            };
            let key = tail[..end].trim();
            let key = if key.starts_with('"') {
                let Ok(key) = serde_json::from_str::<String>(key) else {
                    return Some(Vec::new());
                };
                key
            } else if let Some(key) = key
                .strip_prefix('\'')
                .and_then(|key| key.strip_suffix('\''))
            {
                key.to_string()
            } else if key.chars().all(|c| c.is_ascii_digit()) && !key.is_empty() {
                key.to_string()
            } else {
                return Some(Vec::new());
            };
            path.push(key);
            rest = &tail[end + 1..];
        } else {
            return Some(Vec::new());
        }
    }
    Some(path)
}

/// A type-correct value lets the mock run reach unrelated tool bindings.
fn placeholder(ty: InputType) -> Value {
    match ty {
        InputType::String => json!("preflight"),
        InputType::Number => json!(0),
        InputType::Boolean => json!(false),
        InputType::Json => json!({}),
    }
}

/// Find runtime scope reads outside quoted jq literals. Complex operations
/// conservatively address the whole named scope rather than inventing a value.
pub(super) fn scope_reads(expression: &str) -> Vec<(String, Vec<String>)> {
    let Some(body) = expression.strip_prefix('=') else {
        return vec![];
    };
    if body.trim() == "." {
        return vec![("inputs".into(), vec![]), ("run".into(), vec![])];
    }
    let bytes = body.as_bytes();
    let mut reads = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let nested_dot =
            i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || matches!(bytes[i - 1], b'_' | b']'));
        if bytes[i] == b'.' && !nested_dot {
            if let Some((end, scope)) = root_selector(body, i + 1) {
                match scope {
                    Some(scope)
                        if matches!(
                            scope.as_str(),
                            "inputs" | "run" | "item" | "items" | "nodes"
                        ) =>
                    {
                        let path =
                            input_path(&format!("=inputs{}", &body[end..])).unwrap_or_default();
                        reads.push((scope, path));
                    }
                    None => reads.push(("run".into(), vec![])),
                    _ => {}
                }
                i = end;
                continue;
            }
        }
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i];
            let start = i;
            let root_access = i >= 2 && bytes[i - 1] == b'[' && bytes[i - 2] == b'.';
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 2;
                } else if bytes[i] == quote {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            if root_access && i < bytes.len() && bytes[i] == b']' {
                if let Ok(scope) = serde_json::from_str::<String>(&body[start..i]) {
                    if matches!(
                        scope.as_str(),
                        "inputs" | "run" | "item" | "items" | "nodes"
                    ) {
                        let path =
                            input_path(&format!("=inputs{}", &body[i + 1..])).unwrap_or_default();
                        reads.push((scope, path));
                    }
                }
            }
        } else if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let scope = &body[start..i];
            let nested = start >= 2
                && bytes[start - 1] == b'.'
                && (bytes[start - 2].is_ascii_alphanumeric()
                    || matches!(bytes[start - 2], b'_' | b']'));
            let root = body[..start].trim().is_empty()
                || (start > 0 && bytes[start - 1] == b'.' && !nested)
                || (!nested
                    && body[i..].starts_with(['.', '['])
                    && (start == 0 || bytes[start - 1] != b'$'));
            let object_key = body[i..].trim_start().starts_with(':');
            if root && !object_key && matches!(scope, "inputs" | "run" | "item" | "items" | "nodes")
            {
                let path = input_path(&format!("=inputs{}", &body[i..])).unwrap_or_default();
                reads.push((scope.to_owned(), path));
            }
        } else {
            i += 1;
        }
    }
    reads
}

/// Resolve a root jq selector when it is constant; unknown selectors can read any runtime scope.
fn root_selector(body: &str, mut start: usize) -> Option<(usize, Option<String>)> {
    let bytes = body.as_bytes();
    while bytes.get(start).is_some_and(u8::is_ascii_whitespace) {
        start += 1;
    }
    if bytes.get(start) != Some(&b'[') {
        return None;
    }
    let mut depth = 1;
    let mut quote = None;
    let mut end = start + 1;
    while end < bytes.len() {
        let byte = bytes[end];
        if let Some(delimiter) = quote {
            if byte == b'\\' {
                end += 2;
                continue;
            }
            if byte == delimiter {
                quote = None;
            }
        } else {
            match byte {
                b'"' | b'\'' => quote = Some(byte),
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        // Only a literal selector proves the addressed scope.
                        // Evaluating a computed selector against empty mocks can
                        // choose a different branch than real caller data does.
                        let scope = serde_json::from_str::<String>(&body[start + 1..end]).ok();
                        return Some((end + 1, scope));
                    }
                }
                _ => {}
            }
        }
        end += 1;
    }
    None
}
