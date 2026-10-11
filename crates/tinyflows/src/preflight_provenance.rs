//! Field provenance for outputs derived from unavailable caller data.

use super::input_scope::{MockInputs, scope_reads};
use crate::model::{NodeKind, WorkflowGraph};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

type Paths = Vec<Vec<String>>;

#[derive(Clone, Default, PartialEq, Eq)]
struct Output {
    opaque: bool,
    paths: Paths,
    // Literal overlays close these fields even when the remaining payload is opaque.
    known: HashSet<String>,
}

impl Output {
    fn is_unknown(&self, path: &[String]) -> bool {
        intersects(&self.paths, path)
            || (self.opaque && !path.first().is_some_and(|field| self.known.contains(field)))
    }
}

pub(super) struct Provenance<'a> {
    graph: &'a WorkflowGraph,
    inputs: &'a MockInputs,
    outputs: HashMap<String, Output>,
}

impl<'a> Provenance<'a> {
    pub(super) fn new(graph: &'a WorkflowGraph, inputs: &'a MockInputs) -> Self {
        let mut result = Self {
            graph,
            inputs,
            outputs: HashMap::new(),
        };
        // A fixed point handles node declaration order and dependent chains.
        for _ in 0..=graph.nodes.len() {
            let mut changed = false;
            for node in &graph.nodes {
                let mut output = Output::default();
                if matches!(node.kind, NodeKind::Trigger | NodeKind::SubWorkflow) {
                    output.opaque = true;
                } else if matches!(
                    node.kind,
                    NodeKind::Transform
                        | NodeKind::Condition
                        | NodeKind::Switch
                        | NodeKind::Merge
                        | NodeKind::Loop
                        | NodeKind::Dedup
                ) {
                    let predecessors: Vec<_> = graph
                        .edges
                        .iter()
                        .filter(|edge| edge.to_node == node.id)
                        .filter_map(|edge| result.outputs.get(&edge.from_node))
                        .collect();
                    if let Some(first) = predecessors.first() {
                        output.known = first.known.clone();
                    }
                    for predecessor in predecessors {
                        output.opaque |= predecessor.opaque;
                        output.paths.extend(predecessor.paths.iter().cloned());
                        output
                            .known
                            .retain(|field| predecessor.known.contains(field));
                    }
                    // TransformNode overlays only `set`, preserving all other fields.
                    if let Some(Value::Object(fields)) = node
                        .config
                        .get("set")
                        .filter(|_| node.kind == NodeKind::Transform)
                    {
                        for (field, value) in fields {
                            output.paths.retain(|path| path.first() != Some(field));
                            if value.as_str().is_some_and(|expression| {
                                result.is_unverifiable(expression, &node.id)
                            }) {
                                output.known.remove(field);
                                output.paths.push(vec![field.clone()]);
                            } else {
                                output.known.insert(field.clone());
                            }
                        }
                    }
                } else if result.value_is_unknown(&node.config, &node.id) {
                    output.opaque = true;
                }
                output.paths.sort();
                output.paths.dedup();
                if result.outputs.get(&node.id) != Some(&output) {
                    result.outputs.insert(node.id.clone(), output);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        result
    }

    fn value_is_unknown(&self, value: &Value, node: &str) -> bool {
        match value {
            Value::String(expression) => self.is_unverifiable(expression, node),
            Value::Object(values) => values
                .values()
                .any(|value| self.value_is_unknown(value, node)),
            Value::Array(values) => values
                .iter()
                .any(|value| self.value_is_unknown(value, node)),
            _ => false,
        }
    }

    pub(super) fn is_unverifiable(&self, expression: &str, node: &str) -> bool {
        if !crate::expr::is_expression(expression) {
            return false;
        }
        let trigger = self
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == NodeKind::Trigger)
            .map(|node| node.id.as_str());
        if super::is_trigger_scoped_expression(expression, self.graph, node, trigger) {
            return true;
        }
        scope_reads(expression)
            .iter()
            .any(|(scope, path)| match scope.as_str() {
                "inputs" => intersects(&self.inputs.unknown, path),
                "run" => true,
                "nodes" => {
                    let Some((id, rest)) = path.split_first() else {
                        return self.outputs.values().any(|output| output.is_unknown(&[]));
                    };
                    self.outputs
                        .get(id)
                        .is_some_and(|output| output.is_unknown(output_path(rest)))
                }
                "item" | "items" => self
                    .graph
                    .edges
                    .iter()
                    .filter(|edge| edge.to_node == node)
                    .any(|edge| {
                        self.outputs.get(&edge.from_node).is_some_and(|output| {
                            let path = if scope == "items"
                                && path.first().is_some_and(|key| key.parse::<usize>().is_ok())
                            {
                                &path[1..]
                            } else {
                                path.as_slice()
                            };
                            output.is_unknown(path)
                        })
                    }),
                _ => false,
            })
    }
}

fn output_path(path: &[String]) -> &[String] {
    match path.first().map(String::as_str) {
        Some("item") => &path[1..],
        Some("items") if path.get(1).is_some_and(|key| key.parse::<usize>().is_ok()) => &path[2..],
        _ => path,
    }
}

fn intersects(unknown: &Paths, path: &[String]) -> bool {
    unknown
        .iter()
        .any(|unknown| path.starts_with(unknown) || unknown.starts_with(path))
}
