//! What this host will actually permit a workflow to do.
//!
//! The grounding an author most needs and cannot derive. Every fact here is
//! enforced at **run** time by whoever runs the graph, so a graph that ignores
//! one saves cleanly, validates cleanly, and then fails the first time it
//! matters — usually overnight, to nobody watching.
//!
//! Two uses, and both matter:
//!
//! * **rendered into the authoring prompt**, so the model writes something this
//!   machine can run; and
//! * **checked after authoring**, because a prompt is a request and a check is
//!   a fact. The model will name a worker that does not exist however clearly
//!   the list was given.
//!
//! **An absent fact means unknown, never forbidden.** A host that supplies no
//! worker list gets no worker check — not every graph refused. The opposite
//! reading turns an unconfigured host into one that can run nothing, and the
//! symptom is every authored graph failing for a reason the operator never set.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tinyflows::model::{NodeKind, WorkflowGraph};

/// What a host permits. Read from that host's configuration, never guessed.
///
/// Construct it with [`HostFacts::unknown`] and fill in what is actually known:
/// One callable tool, with the argument shape a `tool_call` node must send.
///
/// The engine's tool capability takes `args` as an opaque value, so the only
/// place an author can learn a tool's argument names is here. A slug listed
/// without a fact is a tool the model can only misuse — observed in the
/// field as an author inventing `args.command` for a shell tool, twice,
/// spending the whole episode on a key name it was never shown.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolFact {
    /// The slug a `tool_call` node's config names.
    pub slug: String,
    /// The arguments it takes, in prose an author can follow: key names,
    /// which are required, and what each means.
    pub args: String,
}

/// every collection left empty and every `Option` left `None` disables its own
/// check rather than failing it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostFacts {
    /// Where an `agent` node with no `agent_ref` goes. `None` makes
    /// `agent_ref` **mandatory on every agent node** — a host fact that
    /// changes a field from optional to required, which is why it cannot be
    /// left to the model to infer.
    pub default_worker: Option<String>,
    /// Workers an `agent_ref` may name. Empty means the list is unknown.
    pub workers: Vec<String>,
    /// Harness names this host understands, built in or configured.
    pub harnesses: Vec<String>,
    /// The harness used when a node and the document both stay silent.
    pub default_harness: Option<String>,
    /// The model used when a node and the document both stay silent.
    pub default_model: Option<String>,
    /// Tool slugs that resolve without an allowlist entry.
    pub native_tools: Vec<String>,
    /// Slugs permitted beyond the native ones. Empty *and* `native_tools`
    /// empty means slugs are unchecked.
    pub tool_allowlist: Vec<String>,
    /// Argument documentation for the tools worth documenting.
    ///
    /// Additive: a slug may appear in `native_tools` without a fact here —
    /// that is "callable, shape unknown", which is what every host said
    /// before this field existed.
    #[serde(default)]
    pub tools: Vec<ToolFact>,
    /// Hosts `http_request` may reach. Empty means unchecked.
    pub http_allowlist: Vec<String>,
    /// Whether `code` nodes run at all. `None` means unknown.
    pub allow_code: Option<bool>,
    /// Whether a `shell` step may use a POSIX shell. `None` means unknown;
    /// `Some(false)` is a Windows host, where `shell` is refused rather than
    /// emulated.
    pub shell_available: Option<bool>,
    /// Trigger kinds that actually dispatch here. Empty means unchecked — and
    /// a host that stores nine kinds while firing one should say so, because
    /// the others save and validate and never run.
    pub trigger_kinds: Vec<String>,
    /// The host's own ceiling, which a graph's `max_iterations` sits under.
    pub max_loop_iterations: Option<u64>,
    /// How many `agent` nodes may run at once.
    pub max_parallel_agents: Option<u32>,
    /// How long a whole run may take.
    pub run_timeout_secs: Option<u64>,
    /// Consequences of the facts above, in prose.
    ///
    /// Carried beside the data rather than derived from it because the
    /// consequence is what the model needs: `default_worker: null` is a fact,
    /// "every agent node must name `agent_ref`" is the instruction, and only
    /// the host knows which of its facts have consequences worth stating.
    pub notes: Vec<String>,
}

impl HostFacts {
    /// A host that has told us nothing. Every check is skipped.
    #[must_use]
    pub fn unknown() -> Self {
        Self::default()
    }

    /// Whether anything here is worth showing an author.
    #[must_use]
    pub fn is_unknown(&self) -> bool {
        self.default_worker.is_none()
            && self.workers.is_empty()
            && self.harnesses.is_empty()
            && self.default_harness.is_none()
            && self.default_model.is_none()
            && self.max_parallel_agents.is_none()
            && self.run_timeout_secs.is_none()
            && self.native_tools.is_empty()
            && self.tool_allowlist.is_empty()
            && self.tools.is_empty()
            && self.http_allowlist.is_empty()
            && self.allow_code.is_none()
            && self.shell_available.is_none()
            && self.trigger_kinds.is_empty()
            && self.max_loop_iterations.is_none()
            && self.notes.is_empty()
    }

    /// Everything about `graph` this host would refuse, all at once.
    ///
    /// Every failure rather than the first, for the same reason the validator
    /// reports every failure: a model handed one problem fixes it and returns
    /// with the next.
    #[must_use]
    pub fn check(&self, graph: &WorkflowGraph) -> Vec<String> {
        let mut problems = Vec::new();
        for node in &graph.nodes {
            match node.kind {
                NodeKind::Agent => self.check_agent(node, &mut problems),
                NodeKind::ToolCall => self.check_tool(node, &mut problems),
                NodeKind::HttpRequest => self.check_http(node, &mut problems),
                NodeKind::Code => self.check_code(node, &mut problems),
                NodeKind::Shell => self.check_shell(node, &mut problems),
                NodeKind::Loop => self.check_loop(node, &mut problems),
                NodeKind::Trigger => self.check_trigger(node, &mut problems),
                _ => {}
            }
        }
        problems
    }

    fn check_agent(&self, node: &tinyflows::model::Node, out: &mut Vec<String>) {
        let named = text(&node.config, "agent_ref");
        match named {
            None => {
                if self.default_worker.is_none() && !self.workers.is_empty() {
                    out.push(format!(
                        "node `{}`: this host has no default worker, so every agent node must \
                         name `config.agent_ref` (one of: {})",
                        node.id,
                        self.workers.join(", ")
                    ));
                }
            }
            Some(reference) => {
                if !self.workers.is_empty() && !self.workers.iter().any(|w| w == reference) {
                    out.push(format!(
                        "node `{}`: no worker named `{reference}` on this host (have: {})",
                        node.id,
                        self.workers.join(", ")
                    ));
                }
            }
        }
        if let Some(harness) = text(&node.config, "harness")
            && !self.harnesses.is_empty()
            && !self.harnesses.iter().any(|h| h == harness)
        {
            out.push(format!(
                "node `{}`: no harness named `{harness}` here (have: {})",
                node.id,
                self.harnesses.join(", ")
            ));
        }
    }

    fn check_tool(&self, node: &tinyflows::model::Node, out: &mut Vec<String>) {
        let Some(slug) = text(&node.config, "slug") else {
            return;
        };
        if self.native_tools.is_empty() && self.tool_allowlist.is_empty() {
            return;
        }
        let known = self.native_tools.iter().chain(self.tool_allowlist.iter());
        if !known.into_iter().any(|s| s == slug) {
            out.push(format!(
                "node `{}`: the tool slug `{slug}` does not resolve here (native: {}; allowed: {})",
                node.id,
                render_list(&self.native_tools),
                render_list(&self.tool_allowlist),
            ));
        }
    }

    fn check_http(&self, node: &tinyflows::model::Node, out: &mut Vec<String>) {
        if self.http_allowlist.is_empty() {
            return;
        }
        let Some(url) = text(&node.config, "url") else {
            return;
        };
        // A URL built from an expression is only known at run time. Refusing
        // it here would refuse the correct way to write a parameterised
        // request, so an unresolvable host is left to run time on purpose.
        if url.starts_with('=') {
            return;
        }
        // DNS names are case-insensitive; an allowlist that rejects
        // `API.GitHub.com` against `github.com` costs the episode a spurious
        // authoring round.
        let Some(host) = host_of(url) else { return };
        let host = host.to_ascii_lowercase();
        if !self
            .http_allowlist
            .iter()
            .map(|allowed| allowed.to_ascii_lowercase())
            .any(|allowed| host == allowed || host.ends_with(&format!(".{allowed}")))
        {
            out.push(format!(
                "node `{}`: this host may not reach `{host}` (allowed: {})",
                node.id,
                self.http_allowlist.join(", ")
            ));
        }
    }

    fn check_code(&self, node: &tinyflows::model::Node, out: &mut Vec<String>) {
        if self.allow_code == Some(false) {
            out.push(format!(
                "node `{}`: `code` nodes are disabled on this host",
                node.id
            ));
        }
    }

    fn check_shell(&self, node: &tinyflows::model::Node, out: &mut Vec<String>) {
        if self.shell_available == Some(false) {
            out.push(format!(
                "node `{}`: this host refuses POSIX shell rather than emulating it — \
                 use a `code` node with javascript or python",
                node.id
            ));
        }
    }

    fn check_loop(&self, node: &tinyflows::model::Node, out: &mut Vec<String>) {
        let (Some(ceiling), Some(asked)) = (
            self.max_loop_iterations,
            node.config.get("max_iterations").and_then(Value::as_u64),
        ) else {
            return;
        };
        if asked > ceiling {
            out.push(format!(
                "node `{}`: max_iterations {asked} is above this host's ceiling of {ceiling}, \
                 so the loop stops earlier than the graph says",
                node.id
            ));
        }
    }

    fn check_trigger(&self, node: &tinyflows::model::Node, out: &mut Vec<String>) {
        if self.trigger_kinds.is_empty() {
            return;
        }
        let kind = text(&node.config, "trigger_kind").unwrap_or("manual");
        if !self.trigger_kinds.iter().any(|k| k == kind) {
            out.push(format!(
                "node `{}`: a `{kind}` trigger is stored but never dispatched here — \
                 this host fires: {}",
                node.id,
                self.trigger_kinds.join(", ")
            ));
        }
    }

    /// The facts as an author should read them.
    ///
    /// Returns an empty string when nothing is known, so a caller can append it
    /// unconditionally without producing an empty heading.
    #[must_use]
    pub fn render(&self) -> String {
        if self.is_unknown() {
            return String::new();
        }
        let mut lines = vec!["# What this host permits — enforced at run time".to_string()];
        let mut say = |label: &str, value: String| {
            if !value.is_empty() {
                lines.push(format!("- {label}: {value}"));
            }
        };

        say(
            "default worker",
            self.default_worker
                .clone()
                .unwrap_or_else(|| "none — every agent node must name config.agent_ref".into()),
        );
        say("workers", render_list(&self.workers));
        say("harnesses", render_list(&self.harnesses));
        say(
            "default harness",
            self.default_harness.clone().unwrap_or_default(),
        );
        say(
            "default model",
            self.default_model.clone().unwrap_or_default(),
        );
        say("tool slugs that resolve", render_list(&self.native_tools));
        say("tool slugs also allowed", render_list(&self.tool_allowlist));
        for tool in &self.tools {
            say(&format!("tool `{}` args", tool.slug), tool.args.clone());
        }
        say("http hosts reachable", render_list(&self.http_allowlist));
        if let Some(allowed) = self.allow_code {
            say(
                "code nodes",
                if allowed {
                    "permitted".into()
                } else {
                    "DISABLED".into()
                },
            );
        }
        if self.shell_available == Some(false) {
            say(
                "posix shell",
                "refused, not emulated — use javascript or python".into(),
            );
        }
        say("triggers that fire", render_list(&self.trigger_kinds));
        if let Some(cap) = self.max_loop_iterations {
            say(
                "loop ceiling",
                format!("{cap} iterations, whatever a graph asks for"),
            );
        }
        if let Some(cap) = self.max_parallel_agents {
            say("agents at once", cap.to_string());
        }
        if let Some(secs) = self.run_timeout_secs {
            say("run timeout", format!("{secs}s"));
        }
        for note in &self.notes {
            lines.push(format!("- {note}"));
        }
        lines.join("\n")
    }
}

fn text<'a>(config: &'a Value, key: &str) -> Option<&'a str> {
    config
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn render_list(items: &[String]) -> String {
    items.join(", ")
}

/// The host part of a URL, without pulling in a URL parser for one field.
fn host_of(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host.split(':').next()?;
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;
