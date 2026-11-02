//! Which trigger kinds fire a flow on their own, and the author-facing warning
//! for the ones that do not.
//!
//! A flow whose trigger is `webhook`, `form`, `chat_message`, ... is accepted
//! and saved, but a host that has not wired that dispatch path never runs it.
//! `graph_trigger_warnings` turns that silence into a non-fatal warning.
//! The set of kinds that fire is `trigger_kind_fires`; a host that wires more
//! kinds filters the warning itself with `graph_trigger_warnings_for`.

use crate::model::{TriggerKind, WorkflowGraph};

/// Stable snake_case label for a [`TriggerKind`], matching its serde wire
/// discriminator. Spelled out (not derived through serde) so the exact string
/// in an author-facing warning is unmistakable at the call site.
#[must_use]
pub fn trigger_kind_label(kind: &TriggerKind) -> &'static str {
    match kind {
        TriggerKind::Manual => "manual",
        TriggerKind::Schedule => "schedule",
        TriggerKind::Webhook => "webhook",
        TriggerKind::AppEvent => "app_event",
        TriggerKind::Form => "form",
        TriggerKind::ExecuteByWorkflow => "execute_by_workflow",
        TriggerKind::ChatMessage => "chat_message",
        TriggerKind::Evaluation => "evaluation",
        TriggerKind::System => "system",
    }
}

/// Whether a trigger kind produces runs without a human pressing "run" in the
/// reference host wiring: `manual` (on demand), `schedule` (a timer tick) and
/// `app_event` (a connected-app event). Everything else is accepted and saved
/// but has no dispatch path.
#[must_use]
pub fn trigger_kind_fires(kind: &TriggerKind) -> bool {
    matches!(
        kind,
        TriggerKind::Manual | TriggerKind::Schedule | TriggerKind::AppEvent
    )
}

/// The graph's single trigger node's `trigger_kind`, when it has one that
/// parses. `None` for a graph without exactly one trigger node, a trigger with
/// no `trigger_kind` (a legacy manual-only graph: it never self-fires, which is
/// not a surprise worth a warning), or an unrecognised discriminator.
#[must_use]
pub fn graph_trigger_kind(graph: &WorkflowGraph) -> Option<TriggerKind> {
    let trigger = graph.trigger()?;
    let value = trigger.config.get("trigger_kind")?;
    serde_json::from_value(value.clone()).ok()
}

/// Whether the graph has exactly one `app_event` trigger bound to the given
/// toolkit and trigger slug. Both identifiers are matched case-insensitively.
#[must_use]
pub fn app_event_binding(graph: &WorkflowGraph, toolkit: &str, trigger_slug: &str) -> bool {
    let Some(trigger) = graph.trigger() else {
        return false;
    };
    if graph_trigger_kind(graph) != Some(TriggerKind::AppEvent) {
        return false;
    }
    trigger
        .config
        .get("toolkit")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| value.eq_ignore_ascii_case(toolkit))
        && trigger
            .config
            .get("trigger_slug")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value.eq_ignore_ascii_case(trigger_slug))
}

/// Literal object values pinned under the graph's single trigger `inputs`.
/// Missing, multiple, or non-object trigger inputs produce an empty map; values
/// are configuration and are never interpreted as expressions.
#[must_use]
pub fn pinned_inputs(graph: &WorkflowGraph) -> serde_json::Map<String, serde_json::Value> {
    graph
        .trigger()
        .and_then(|trigger| trigger.config.get("inputs"))
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// Non-fatal validation warnings for a graph, using [`trigger_kind_fires`] as
/// the set of wired kinds. Today exactly one warning exists: the trigger kind
/// does not fire automatically. Empty when it does, or when the graph has no
/// single resolvable trigger kind (see [`graph_trigger_kind`]).
#[must_use]
pub fn graph_trigger_warnings(graph: &WorkflowGraph) -> Vec<String> {
    graph_trigger_warnings_for(graph, trigger_kind_fires)
}

/// [`graph_trigger_warnings`] for a host that has wired its own set of trigger
/// kinds: `fires` says which kinds produce runs on that host.
#[must_use]
pub fn graph_trigger_warnings_for(
    graph: &WorkflowGraph,
    fires: impl Fn(&TriggerKind) -> bool,
) -> Vec<String> {
    let Some(kind) = graph_trigger_kind(graph) else {
        return Vec::new();
    };
    if fires(&kind) {
        return Vec::new();
    }
    let label = trigger_kind_label(&kind);
    vec![format!(
        "Trigger kind '{label}' does not fire automatically yet — this flow will be saved and \
         can be enabled, but nothing will run it on its own until that trigger is wired up. Run \
         it manually with flows_run, or switch to a `schedule` or `app_event` trigger."
    )]
}

#[cfg(test)]
#[path = "triggers_tests.rs"]
mod tests;
