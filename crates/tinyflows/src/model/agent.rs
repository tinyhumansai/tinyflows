//! Declarative agent types: what an `agent` node runs, and how it is configured.
//!
//! An [`AgentDefinition`] is a **reusable agent type** — instructions, model,
//! provider, granted tools, standing context, and advisory limits. Definitions
//! come from two places, and an `agent` node's `agent_ref` selects one:
//!
//! | source | declared in | resolved by | portable |
//! |---|---|---|---|
//! | in-graph registry | [`WorkflowGraph::agents`](crate::model::WorkflowGraph::agents) | the engine | yes — travels with the workflow |
//! | host registry | the embedding harness | [`AgentRunner::resolve_agent`](crate::caps::AgentRunner::resolve_agent) | no — host-specific |
//!
//! The in-graph registry wins on a collision, so a workflow that carries its own
//! definition behaves the same on every host. An `agent_ref` that neither
//! resolves is **not** an error: it is passed through to the harness as a
//! minimal `AgentDefinition { id: agent_ref, .. }`, preserving the pre-registry
//! behavior where `agent_ref` was an opaque string the host interpreted.
//!
//! ## What this module is not
//!
//! tinyflows implements **no agentic loop**. These types describe an agent
//! *declaratively* so the engine can assemble a
//! [`AgentRunRequest`](crate::caps::AgentRunRequest); the model↔tool loop, the
//! transcript, and the prompt assembly all belong to the harness. Everything
//! here is `serde` + `std` only, so a host can implement against it without
//! linking the engine.
//!
//! ## Trusted config only
//!
//! [`ToolGrant::slug`], [`ToolGrant::connection_ref`], a node's `agent_ref`, and
//! [`ContextSourceKind::Memory`]'s `scope` are **literals, never
//! `=`-expressions**. An expression there would let upstream data — which may
//! include model output — choose which credential a call acts as, which tool it
//! reaches, or which agent type (and therefore which tool grants) a turn runs
//! with. [`crate::validate`] rejects those structurally, before a run starts.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A reusable agent type: what the agent is, what it may use, and how far it
/// may go.
///
/// Every field but `id` defaults, so a definition can be as thin as
/// `{"id": "researcher"}` — useful when the harness holds the real
/// configuration and the graph only needs to name it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AgentDefinition {
    /// Stable id an `agent` node's `agent_ref` selects, unique within a graph.
    ///
    /// Opaque to the engine: compared for equality and echoed back to the
    /// harness, never parsed.
    pub id: String,

    /// Human-readable name for editors and pickers.
    ///
    /// Never an identity — resolve on [`id`](Self::id), never on this.
    #[serde(default)]
    pub name: String,

    /// What this agent is for. Surfaced to authors, and passed to the harness
    /// (which may show it to a delegating parent agent).
    #[serde(default)]
    pub description: String,

    /// The agent's standing instructions — its contribution to the system
    /// prompt.
    ///
    /// Passed to the harness verbatim. The engine never truncates it and never
    /// composes a prompt of its own: prompt assembly belongs to whoever owns the
    /// loop. May contain `=`-expressions, which the engine resolves first.
    #[serde(default)]
    pub instructions: Option<String>,

    /// Preferred model id, opaque to the engine (`"claude-opus-5"`, a host
    /// alias, whatever the harness understands).
    ///
    /// `None` means "no preference, the harness's default applies" — it does not
    /// mean "no model". Encoding the fallback as absence keeps the choice of a
    /// default with the only party that has credentials for one.
    #[serde(default)]
    pub model: Option<String>,

    /// Preferred provider id, opaque to the engine (`"anthropic"`, a gateway
    /// name, a host routing key).
    ///
    /// Carried separately from [`model`](Self::model) rather than folded into
    /// it, because a harness routing the same model id through different
    /// providers should not have to parse a composite string apart.
    #[serde(default)]
    pub provider: Option<String>,

    /// Working directory the agent should run in, when it runs somewhere with a
    /// filesystem — a repo checkout, a sandbox root, a scratch workspace.
    ///
    /// Optional and opaque: the engine never touches the filesystem, never
    /// canonicalizes this, and never checks that it exists. `None` means "no
    /// author preference"; the harness's own default applies.
    ///
    /// This is **untrusted authoring input**, exactly like the path fields on
    /// [`ShellRequest`](crate::caps::ShellRequest). It may be an
    /// `=`-expression — a checkout path produced by an upstream node is the
    /// motivating case — so a harness must validate it against whatever roots it
    /// permits before handing it to a process, rather than assuming the engine
    /// vetted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,

    /// Advisory ceilings on the harness's loop. See [`AgentLimits`].
    #[serde(default)]
    pub limits: AgentLimits,

    /// The tools this agent type may use, before any node-level narrowing.
    #[serde(default)]
    pub tools: Vec<ToolGrant>,

    /// Context composed into every run of this agent type, in declaration
    /// order.
    #[serde(default)]
    pub context: Vec<ContextSource>,

    /// Free-form passthrough for the harness: tier hints, sandbox policy, UI
    /// affordances, anything this crate has no business modelling.
    ///
    /// The engine never interprets it — it merges node-level keys over these
    /// and forwards the result on the request.
    #[serde(default)]
    pub metadata: Map<String, Value>,
}

impl AgentDefinition {
    /// A definition carrying nothing but an id.
    ///
    /// This is what an unresolved `agent_ref` becomes: the harness receives the
    /// id it was given and resolves the rest internally.
    ///
    /// ```
    /// use tinyflows::model::AgentDefinition;
    ///
    /// let agent = AgentDefinition::new("researcher");
    /// assert_eq!(agent.id, "researcher");
    /// assert!(agent.tools.is_empty());
    /// assert!(agent.model.is_none());
    /// ```
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Self::default()
        }
    }
}

/// Advisory bounds on a harness-owned agent loop.
///
/// **Advisory, not enforced.** The loop runs entirely host-side, so the engine
/// never observes a step or a tool call and cannot enforce a cap on either.
/// These are the author's declared intent, forwarded so the harness can apply
/// them; a harness that ignores them is not violating a contract this crate
/// could have checked. What the engine *does* enforce is its own
/// `node_timeout_secs`, which bounds the whole capability call regardless.
///
/// A `None` field means "no author-declared bound", not "unbounded" — the
/// harness's own default applies.
///
/// The two timeouts are separate on purpose. A whole-run budget cannot express
/// "no single tool call may hang for more than 30s", and a per-tool timeout
/// cannot express "give up on this agent after five minutes" — a loop making
/// steady, individually-fast tool calls runs forever under a per-tool bound
/// alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AgentLimits {
    /// Maximum model↔tool iterations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u32>,
    /// Maximum individual tool invocations across the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// Wall-clock budget, in seconds, for the **whole agent run** — every model
    /// call, every tool call, and the harness's own overhead between them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_timeout_secs: Option<u32>,
    /// Wall-clock budget, in seconds, for a **single tool invocation**.
    ///
    /// Bounds each call independently, so one wedged tool cannot consume the
    /// whole agent budget. How a harness reports the timeout to the model — as a
    /// tool error it can recover from, or as a hard stop — is its own policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_secs: Option<u32>,
}

impl AgentLimits {
    /// Whether no bound at all is declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.max_steps.is_none()
            && self.max_tool_calls.is_none()
            && self.agent_timeout_secs.is_none()
            && self.tool_timeout_secs.is_none()
    }

    /// Narrows `self` by `other`, taking the **lower** of each declared bound.
    ///
    /// Used to apply a node's limits to its agent definition's: a node may only
    /// tighten a ceiling the definition set, never raise it, so a curated agent
    /// type cannot be widened by the graph that uses it.
    ///
    /// ```
    /// use tinyflows::model::AgentLimits;
    ///
    /// let definition = AgentLimits {
    ///     max_steps: Some(8), max_tool_calls: Some(20),
    ///     agent_timeout_secs: None, tool_timeout_secs: Some(60),
    /// };
    /// let node = AgentLimits {
    ///     max_steps: Some(4), max_tool_calls: Some(99),
    ///     agent_timeout_secs: Some(30), tool_timeout_secs: None,
    /// };
    ///
    /// let merged = definition.narrowed_by(&node);
    /// assert_eq!(merged.max_steps, Some(4));            // node tightened it
    /// assert_eq!(merged.max_tool_calls, Some(20));      // node tried to widen it; ignored
    /// assert_eq!(merged.agent_timeout_secs, Some(30));  // only the node declared one
    /// assert_eq!(merged.tool_timeout_secs, Some(60));   // only the definition declared one
    /// ```
    #[must_use]
    pub fn narrowed_by(&self, other: &Self) -> Self {
        /// The lower of two optional bounds, or whichever one is declared.
        fn lower(a: Option<u32>, b: Option<u32>) -> Option<u32> {
            match (a, b) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (some, None) | (None, some) => some,
            }
        }
        Self {
            max_steps: lower(self.max_steps, other.max_steps),
            max_tool_calls: lower(self.max_tool_calls, other.max_tool_calls),
            agent_timeout_secs: lower(self.agent_timeout_secs, other.agent_timeout_secs),
            tool_timeout_secs: lower(self.tool_timeout_secs, other.tool_timeout_secs),
        }
    }
}

/// A tool an agent is permitted to call.
///
/// A grant is *intent*, not a descriptor: it names a tool but carries no
/// argument schema, because an author does not write one. The harness turns
/// grants into concrete
/// [`ToolDescriptor`](crate::caps::ToolDescriptor)s through
/// [`AgentRunner::resolve_tools`](crate::caps::AgentRunner::resolve_tools).
///
/// This is a strict superset of the `{"slug": …, "connection_ref": …}` objects
/// an `agent` node's `config.tools` already carried, so existing graphs
/// deserialize unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolGrant {
    /// The tool identifier the harness resolves — an exact slug
    /// (`"github.add_labels"`), or a namespace pattern with a trailing `.*`
    /// (`"github.*"`).
    ///
    /// The engine performs no matching of its own beyond exact-equality subset
    /// checks; expanding a pattern is the harness's job, and a harness that does
    /// not expand receives the pattern verbatim.
    ///
    /// A **literal**, never an `=`-expression: a tool chosen by run data is a
    /// tool chosen by whatever wrote that data.
    pub slug: String,

    /// Trusted connection reference for calls to this tool, overriding the
    /// node's.
    ///
    /// Trusted config only — a model-supplied connection reference is never
    /// honored, since it would let a prompt injection point a call at an
    /// arbitrary host credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_ref: Option<String>,
}

impl ToolGrant {
    /// A grant for `slug` with no connection override.
    #[must_use]
    pub fn new(slug: impl Into<String>) -> Self {
        Self {
            slug: slug.into(),
            connection_ref: None,
        }
    }

    /// Whether this grant is a trailing-`.*` namespace pattern rather than an
    /// exact slug.
    ///
    /// ```
    /// use tinyflows::model::ToolGrant;
    ///
    /// assert!(ToolGrant::new("github.*").is_pattern());
    /// assert!(!ToolGrant::new("github.add_labels").is_pattern());
    /// ```
    #[must_use]
    pub fn is_pattern(&self) -> bool {
        self.slug.ends_with(".*")
    }

    /// Whether this grant covers `slug` — by exact match, or by namespace when
    /// this grant is a pattern.
    ///
    /// ```
    /// use tinyflows::model::ToolGrant;
    ///
    /// assert!(ToolGrant::new("github.*").covers("github.add_labels"));
    /// assert!(!ToolGrant::new("github.*").covers("slack.post"));
    /// assert!(ToolGrant::new("slack.post").covers("slack.post"));
    /// ```
    #[must_use]
    pub fn covers(&self, slug: &str) -> bool {
        match self.slug.strip_suffix('*') {
            Some(prefix) => slug.starts_with(prefix),
            None => self.slug == slug,
        }
    }
}

/// One declared source of dynamic context for an agent turn.
///
/// Context is *declarative*: the author says where material comes from, and the
/// engine resolves each source into a
/// [`ContextBlock`](crate::caps::ContextBlock) at request-assembly time. The
/// engine never reorders, merges, de-duplicates, or truncates blocks — composing
/// them into a prompt is the harness's job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextSource {
    /// Stable label for this block, used in the assembled request and in
    /// diagnostics so an author can tell which block failed to resolve. A
    /// generated `"context_<n>"` is used when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    /// Where the material comes from.
    #[serde(flatten)]
    pub kind: ContextSourceKind,

    /// Whether this block is survivable when it cannot be resolved.
    ///
    /// **Defaults to `false`, and that default is load-bearing.** An agent
    /// silently missing its identity text, or the list of integrations the user
    /// has actually connected, still runs and produces confident, wrong output —
    /// a failure that passes a smoke test. So an unresolvable block fails the
    /// node unless the author explicitly said it was optional.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

impl ContextSource {
    /// A required context source of the given kind, with no label.
    #[must_use]
    pub fn new(kind: ContextSourceKind) -> Self {
        Self {
            label: None,
            kind,
            optional: false,
        }
    }

    /// This source's label, or the best available fallback.
    ///
    /// An unlabelled source falls back to whatever identifies it — a host
    /// source's name, a flavour's slug, a memory scope — and only to the
    /// positional `"context_<index>"` when the kind carries no identifier of its
    /// own. That fallback is what an author sees in a "could not be resolved"
    /// error, and `context_0` does not tell them which block to go fix.
    ///
    /// ```
    /// use tinyflows::model::{ContextSource, ContextSourceKind};
    /// use serde_json::Value;
    ///
    /// let host = ContextSource::new(ContextSourceKind::Host {
    ///     source: "repo_conventions".into(),
    ///     params: Value::Null,
    /// });
    /// assert_eq!(host.label_or_index(0), "repo_conventions");
    ///
    /// // A kind with nothing to name itself by falls back to its position.
    /// assert_eq!(ContextSource::new(ContextSourceKind::Items).label_or_index(3), "context_3");
    /// ```
    #[must_use]
    pub fn label_or_index(&self, index: usize) -> String {
        if let Some(label) = &self.label {
            return label.clone();
        }
        match &self.kind {
            ContextSourceKind::Host { source, .. } => source.clone(),
            ContextSourceKind::Flavour { slug } => slug.clone(),
            ContextSourceKind::Memory { scope, .. } => format!("memory:{scope}"),
            ContextSourceKind::Text { .. } | ContextSourceKind::Items => {
                format!("context_{index}")
            }
        }
    }
}

/// Where a [`ContextSource`]'s material comes from.
///
/// Closed on purpose: an author may not name an arbitrary resolution mechanism,
/// because a config string that chooses what the engine executes is a second,
/// unreviewed dispatch table. Four kinds resolve against capabilities this crate
/// already has; only [`Host`](ContextSourceKind::Host) reaches the harness for
/// something the engine cannot produce.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextSourceKind {
    /// Literal text, or an `=`-expression resolved against the node's usual
    /// scope (`item` / `items` / `nodes` / `inputs` / `run`) before assembly.
    ///
    /// The workhorse: `{"kind": "text", "text": "=nodes.fetch.item.json.body"}`
    /// hands an agent an upstream node's output as prose.
    Text {
        /// The block body, literal or `=`-expression.
        ///
        /// Typed as a [`Value`] rather than a `String` because an expression
        /// resolves to whatever the data holds: `"=item.count"` yields a number
        /// and `"=item.record"` an object. Rejecting those would make the
        /// commonest binding in the whole feature fail on a type the author
        /// never wrote. A non-string value is rendered as compact JSON for the
        /// block's `text` and carried structurally in its `data`, so a harness
        /// can use whichever it prefers.
        text: Value,
    },

    /// The node's input items, as a JSON array — the commonest way to hand an
    /// agent the rows the workflow just produced.
    Items,

    /// A recall against the host's memory store, through the **existing**
    /// [`MemoryProvider`](crate::caps::MemoryProvider) capability.
    ///
    /// No new capability is introduced for this: a memory-backed context block
    /// is precisely a `recall`, and a second trait that also read memory would
    /// give the `scope` contract two places to drift. Read-only by construction
    /// — there is no context source that writes.
    Memory {
        /// Host-defined memory scope: `"user"`, `"flow"`, or `"flows"`. A
        /// **literal**, never an `=`-expression, matching the `memory` node.
        scope: String,
        /// Free-text recall query; may be an `=`-expression.
        query: String,
        /// Maximum results to fold into the block.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },

    /// A named host "flavour" profile (an ask/persona/style record), through
    /// [`MemoryProvider::flavour`](crate::caps::MemoryProvider::flavour).
    Flavour {
        /// The flavour slug.
        slug: String,
    },

    /// An opaque, host-named source the harness expands: identity/soul text,
    /// a description of the integrations this user has connected, a RAG index,
    /// learned context.
    ///
    /// The engine resolves any `=`-expressions inside `params` and then hands
    /// the reference to
    /// [`AgentRunner::resolve_context`](crate::caps::AgentRunner::resolve_context);
    /// it knows nothing about `source` beyond that the harness may recognize it.
    Host {
        /// Host-defined source id. Opaque; the engine never parses it.
        source: String,
        /// Host-defined parameters, forwarded after expression resolution.
        #[serde(default, skip_serializing_if = "Value::is_null")]
        params: Value,
    },
}

impl ContextSourceKind {
    /// The `snake_case` wire name of this kind, as it appears in JSON and on the
    /// assembled [`ContextBlock`](crate::caps::ContextBlock).
    ///
    /// ```
    /// use tinyflows::model::ContextSourceKind;
    ///
    /// assert_eq!(ContextSourceKind::Items.as_str(), "items");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Items => "items",
            Self::Memory { .. } => "memory",
            Self::Flavour { .. } => "flavour",
            Self::Host { .. } => "host",
        }
    }
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;
