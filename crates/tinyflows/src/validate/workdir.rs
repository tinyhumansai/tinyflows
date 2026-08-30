use super::*;

/// Every spelling an author reaches for when they mean "run this step over
/// there".
///
/// The list is deliberately wide. The failure this check exists to stop is not
/// a typo — it is a plausible key that the engine accepts, persists, validates
/// cleanly, and then ignores, leaving the step running in the run's workspace
/// with nothing anywhere saying otherwise.
const DIRECTORY_KEYS: [&str; 7] = [
    "cwd",
    "workdir",
    "work_dir",
    "working_dir",
    "working_directory",
    "workspace",
    "directory",
];

/// The directory keys a node of `kind` actually reads at run time.
fn honored_by(kind: &NodeKind) -> &'static [&'static str] {
    match kind {
        // `cwd` is the spelling to reach for; `working_dir` is the older one
        // and the name of the field on `AgentDefinition`, so it stays accepted.
        NodeKind::Agent => &["cwd", "working_dir"],
        NodeKind::Shell => &["cwd"],
        // Not a process working directory: it re-pins the *child run's*
        // workspace, which is what the directories inside it resolve against.
        NodeKind::SubWorkflow => &["workspace"],
        // The run-level knob every `cwd` in the graph resolves against.
        NodeKind::Trigger => &["workspace"],
        _ => &[],
    }
}

/// What to tell an author who put a directory key where it does nothing.
fn advice(kind: &NodeKind, key: &str) -> String {
    match kind {
        NodeKind::Agent | NodeKind::Shell => {
            format!("use `cwd` (this node reads `cwd`, not `{key}`)")
        }
        NodeKind::SubWorkflow => {
            format!("use `workspace` to re-pin the child run's workspace, not `{key}`")
        }
        NodeKind::Trigger => format!("use `workspace` to pin the run's workspace, not `{key}`"),
        NodeKind::ToolCall => format!(
            "a tool's working directory goes in `args.cwd`, which the tool itself reads — a \
             top-level `{key}` on the node is never looked at"
        ),
        _ => format!(
            "a {} node has no working directory; put the step that needs one in an `agent` or \
             `shell` node, or re-pin the workspace on the `sub_workflow` node that runs it",
            kind_name(kind)
        ),
    }
}

/// Directory keys that would be silently ignored where they were written.
///
/// A node's `config` is free-form JSON: an unrecognised key is not a
/// deserialization error, it is simply never read. For most keys that is
/// harmless. For this family it is the worst failure the engine can produce —
/// the author has said *where* the work happens, the graph validates, the run
/// goes green, and the work happened somewhere else. When that somewhere else
/// is the primary checkout of a repository rather than the worktree the author
/// prepared, anything the step commits lands on the wrong branch.
///
/// Refused rather than warned, matching how this crate already treats a config
/// key that cannot do what it says (`concurrency` without `per_item`, an
/// unknown `execution`). Nothing is taken away from a graph that worked: a key
/// named here was, by construction, doing nothing.
pub(super) fn validate_working_dirs(graph: &WorkflowGraph, errors: &mut Vec<ValidationError>) {
    for node in &graph.nodes {
        let honored = honored_by(&node.kind);
        for key in DIRECTORY_KEYS {
            if honored.contains(&key) || node.config.get(key).is_none() {
                continue;
            }
            errors.push(ValidationError::InvalidNodeConfig {
                node: node.id.clone(),
                reason: format!(
                    "`{key}` is not read by a {} node and would be silently ignored — the step \
                     would run in the run's workspace instead: {}",
                    kind_name(&node.kind),
                    advice(&node.kind, key)
                ),
            });
        }
    }
}

#[cfg(test)]
#[path = "workdir_tests.rs"]
mod tests;
