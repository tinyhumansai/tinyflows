//! The pure half of turning an `agent` node's request into a model call and the
//! reply back into a node result.
//!
//! Sits beside `agent_request`: that module assembles the
//! declarative [`AgentRunRequest`](crate::caps::AgentRunRequest); this one
//! handles the looser JSON *completion request* a host adapter receives
//! (`prompt`, `messages`, `input_context`, `output_parser`, `model`,
//! `timeout_secs`) — the `input_context` carrier and its size cap, the
//! structured-output steering contract, the tolerant JSON extraction a reply
//! needs when the model wraps its object in prose or a fence, model and
//! timeout precedence, and the single-message flattening a harness turn takes.
//!
//! Everything here is a function of JSON in, JSON or text out. The host keeps
//! provider selection, the concurrency ceiling, and turning
//! [`CompletionMessage`]s into its own chat-message type.

use serde_json::{Value, json};

#[path = "agent_prompt/json_extract.rs"]
mod json_extract;
pub use json_extract::{
    extract_balanced_json, extract_fenced_json_block, extract_structured_json, parse_llm_json,
};

/// One chat message of a completion, in the host-neutral shape
/// [`build_completion_messages`] returns. `role` is always one of `system`,
/// `user`, `assistant` or `tool`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionMessage {
    /// `system`, `user`, `assistant` or `tool`.
    pub role: &'static str,
    /// The message text.
    pub content: String,
}

impl CompletionMessage {
    fn new(role: &'static str, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

/// Builds a completion's chat message list: the node's `messages` array (when
/// non-empty) or its `prompt` string as a single user message, with up to two
/// leading messages prepended in this exact order when present —
/// `input_context` (the upstream data, see [`input_context_block`]) first, then
/// the structured-output steering instruction — so a model reading the
/// conversation top-to-bottom sees "here is your data" before "here is how to
/// format your answer". `input_context` is prepended as a **user**-role message
/// rather than `system`: it is untrusted upstream data (an email or webhook
/// payload, a prior node's output), and giving attacker-influenced content
/// system-role authority would let a crafted payload masquerade as host
/// instructions. The steering message stays `system` — that instruction is
/// ours, not upstream data.
pub fn build_completion_messages(request: &Value) -> Vec<CompletionMessage> {
    let mut messages: Vec<CompletionMessage> =
        match request.get("messages").and_then(Value::as_array) {
            Some(entries) if !entries.is_empty() => entries
                .iter()
                .filter_map(|entry| {
                    let content = entry.get("content").and_then(Value::as_str)?.to_string();
                    let role = entry.get("role").and_then(Value::as_str).unwrap_or("user");
                    Some(CompletionMessage::new(
                        match role {
                            "system" => "system",
                            "assistant" => "assistant",
                            "tool" => "tool",
                            _ => "user",
                        },
                        content,
                    ))
                })
                .collect(),
            _ => {
                let prompt = request
                    .get("prompt")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                vec![CompletionMessage::new("user", prompt)]
            }
        };

    // A separate prelude (not two `insert(0, …)` calls) guarantees
    // `input_context` lands ahead of the steering message whichever is present.
    let mut prelude: Vec<CompletionMessage> = Vec::new();
    if let Some(block) = input_context_block(request) {
        prelude.push(CompletionMessage::new("user", block));
    }
    if let Some(instruction) = structured_output_instruction(request) {
        prelude.push(CompletionMessage::new("system", instruction));
    }

    if !prelude.is_empty() {
        messages.splice(0..0, prelude);
    }
    messages
}

/// Cap on the serialized `input_context` block size (bytes of the pretty-
/// printed JSON) before truncation. Keeps a huge upstream payload (e.g. a
/// large fan-in `=items` array) from blowing the completion's context window;
/// generous enough that ordinary node outputs never hit it.
pub const INPUT_CONTEXT_MAX_LEN: usize = 50_000;

/// Renders an agent-node's `config.input_context` (an explicit `=`-bound
/// carrier for upstream data — see the module doc and
/// `tinyflows-copilot` workflow-builder prompt) into the system-message text
/// both completion paths (`OpenHumanLlm::complete` and
/// `OpenHumanAgentRunner::run_via_harness`) prepend ahead of the node's own
/// prompt/messages.
///
/// Returns `None` when `input_context` is absent or resolved to `null` (an
/// unset or dangling `=`-binding) so a node that doesn't opt in behaves
/// exactly as before this field existed — no injected block, no wording
/// change. This is the fix for the root cause: an `agent` node's only input
/// channel used to be `config.prompt` itself, forcing builders to smuggle
/// data in via a jq `=`-expression woven into prose (e.g. `"=You are given an
/// email: .item. Classify..."`), which is not a valid jq program and silently
/// resolves to `null` — the agent then runs with an empty prompt. An explicit
/// `input_context` binding (a clean `=item` / `=nodes.<id>.item.json`
/// expression) always resolves to real data or `null`, never to an
/// unparseable string, so this path can't repeat that failure.
pub fn input_context_block(request: &Value) -> Option<String> {
    let ctx = request.get("input_context").filter(|v| !v.is_null())?;
    let mut serialized = serde_json::to_string_pretty(ctx).unwrap_or_default();
    if serialized.is_empty() || serialized == "null" {
        return None;
    }
    if serialized.len() > INPUT_CONTEXT_MAX_LEN {
        // Truncate on a char boundary — `serialized` is UTF-8 and a naive byte
        // slice at exactly `INPUT_CONTEXT_MAX_LEN` could land mid-codepoint.
        let mut end = INPUT_CONTEXT_MAX_LEN;
        while !serialized.is_char_boundary(end) {
            end -= 1;
        }
        serialized.truncate(end);
        serialized.push_str("…(truncated)");
    }
    // `input_context` is untrusted upstream data (e.g. an email/webhook
    // payload) that could itself contain a run of backticks. A fixed
    // ```` ``` ```` fence would let such a payload prematurely close the
    // fence and have its own trailing text read as if it were prompt prose
    // rather than inert data. Use a fence one backtick longer than the
    // longest backtick run actually present in the payload — the same
    // "fence-following" convention Markdown renderers use — so the payload
    // can never break out.
    let fence = "`".repeat((longest_backtick_run(&serialized) + 1).max(3));
    Some(format!(
        "Here is the data from the previous step:\n{fence}json\n{serialized}\n{fence}\nUse this \
         data to complete the task described below."
    ))
}

/// Length of the longest run of consecutive backtick characters in `s` (0 if
/// `s` contains none). Used by [`input_context_block`] to size a code fence
/// that the untrusted payload cannot prematurely close.
pub fn longest_backtick_run(s: &str) -> usize {
    s.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

/// Returns true when an agent-node completion `request` asked for structured
/// output: an `output_parser.schema` is configured on the node, or the config
/// sets `response_format: "json"`.
///
/// This is the host-side contract for **agent → tool wiring**: downstream
/// `=item.<field>` bindings only work when the agent's emitted item is a
/// structured object, so an agent feeding a `tool_call` should declare an
/// output schema (or `response_format: "json"`).
pub fn structured_output_requested(request: &Value) -> bool {
    let has_schema = request
        .get("output_parser")
        .and_then(|p| p.get("schema"))
        .is_some_and(|s| !s.is_null());
    let json_format = request.get("response_format").and_then(Value::as_str) == Some("json");
    has_schema || json_format
}

/// Builds the JSON-steering instruction that a structured-output node needs (an
/// `output_parser.schema` or `response_format: "json"`), or `None` when the node
/// didn't request structured output. The harness path appends it to the run
/// prompt because `run_single` takes a single user message.
pub fn structured_output_instruction(request: &Value) -> Option<String> {
    if !structured_output_requested(request) {
        return None;
    }
    let schema = request.get("output_parser").and_then(|p| p.get("schema"));
    let schema = schema.filter(|s| !s.is_null());
    // Name the kind the schema asks for: an array schema must not say "object".
    let is_array = schema.and_then(|s| s.get("type")).and_then(Value::as_str) == Some("array");
    let kind = if is_array { "array" } else { "object" };
    let mut instruction =
        format!("Respond with a single JSON {kind} only — no prose, no markdown code fences.");
    if let Some(schema) = schema {
        instruction.push_str(&format!(
            " The {kind} must match this JSON Schema:\n{schema}"
        ));
    }
    Some(instruction)
}

/// Renders an agent-node completion `request` into the single user message
/// `OpenHumanSessionHost::run_single` takes: the
/// `prompt` string when present and non-empty, else the `messages` array
/// flattened to `"<role>: <content>"` lines (blank entries skipped). Empty
/// string when neither yields content. Mirrors how `OpenHumanLlm::complete`
/// reads `prompt`/`messages`, collapsed to one string because the harness turn
/// entry point is single-message.
pub fn node_request_to_prompt(request: &Value) -> String {
    if let Some(prompt) = request.get("prompt").and_then(Value::as_str) {
        let prompt = prompt.trim();
        if !prompt.is_empty() {
            return prompt.to_string();
        }
    }
    if let Some(entries) = request.get("messages").and_then(Value::as_array) {
        let parts: Vec<String> = entries
            .iter()
            .filter_map(|entry| {
                let content = entry.get("content").and_then(Value::as_str)?.trim();
                if content.is_empty() {
                    return None;
                }
                let role = entry.get("role").and_then(Value::as_str).unwrap_or("user");
                Some(format!("{role}: {content}"))
            })
            .collect();
        if !parts.is_empty() {
            return parts.join("\n\n");
        }
    }
    String::new()
}

/// Model precedence for an agent node, returning the raw model string as
/// written:
/// 1. node `config.model` — a managed tier (`hint:reasoning`, `hint:chat`, …) or a
///    `hint:*` alias;
/// 2. the registry `entry_model` (custom agents);
/// 3. `None` — no override, so the harness definition's / role default stands.
///
/// Routing translation (tier → workload) happens at application time via
/// `harness_model_default_override`; this function is only the precedence pick,
/// so it stays config-free and trivially testable.
pub fn resolve_node_model(request: &Value, entry_model: Option<&str>) -> Option<String> {
    if let Some(node_model) = request
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        return Some(node_model.to_string());
    }
    entry_model
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
}

/// The model an `agent` node completion actually runs on.
///
/// `resolved_model` is what the host's provider factory returned for the node's
/// workload role. A node may instead pin a **raw/BYOK** model id (for example
/// `claude-opus-4`) that the role mapping collapsed to the default chat role; in
/// that case the pinned id, not the role default, is the model the user
/// selected, so it is forwarded verbatim. Managed tiers and every `hint:*` alias
/// fall through to `resolved_model` unchanged.
///
/// Which ids count as raw passthrough is the host's vocabulary (`is_raw`), so
/// this stays a pure precedence pick.
pub fn resolve_completion_model(
    node_model: Option<&str>,
    resolved_model: String,
    is_raw: impl Fn(&str) -> bool,
) -> String {
    match node_model {
        Some(pinned) if is_raw(pinned) => pinned.to_string(),
        _ => resolved_model,
    }
}

/// Builds `OpenHumanAgentRunner::run_via_harness`'s single run message: the
/// node's `input_context` (when present — see [`input_context_block`]'s doc),
/// then the JSON-steering instruction (when the node requested structured
/// output), then the node's own prompt (or flattened messages, via
/// [`node_request_to_prompt`]). Each present part is separated by a blank
/// line; an absent part contributes nothing (no stray blank lines). Pulled
/// out as its own pure function — rather than inlined in `run_via_harness` —
/// so the prepend order is unit-testable without building a real harness
/// `Agent`.
pub fn build_harness_run_prompt(request: &Value) -> String {
    let parts = [
        input_context_block(request),
        structured_output_instruction(request),
        Some(node_request_to_prompt(request)).filter(|p| !p.is_empty()),
    ];
    parts.into_iter().flatten().collect::<Vec<_>>().join("\n\n")
}

/// Shapes an agent-node harness turn's final text into the node's output value,
/// mirroring `OpenHumanLlm::complete`: when the node requested structured
/// output and the text parses as JSON, the parsed object/array is returned so
/// downstream `=item.<field>` / `=nodes.<id>.item.<field>` bindings work;
/// otherwise `{ text, agent_ref }`. The vendor `agent` node then folds this into
/// the stable `{ json, text, raw }` envelope, and the `output_parser` sub-port
/// still applies.
pub fn build_agent_result(agent_ref: &str, final_text: &str, request: &Value) -> Value {
    if structured_output_requested(request) {
        if let Some(parsed) = extract_structured_json(final_text) {
            tracing::debug!(
                target: "tinyflows::agent_prompt",
                agent_ref,
                "[flows] agent_runner: structured output extracted from harness turn"
            );
            return parsed;
        }
        tracing::warn!(
            target: "tinyflows::agent_prompt",
            agent_ref,
            "[flows] agent_runner: structured output requested but none of the extraction strategies \
             produced valid JSON — falling back to the {{text}} shape (the output_parser sub-port may \
             still coerce it)"
        );
    }
    json!({ "text": final_text, "agent_ref": agent_ref })
}

/// The wall-clock timeout for one agent-node harness turn: the node's requested
/// `timeout_secs` clamped to `10..=600`, defaulting to `240` when unset. A hung
/// provider/tool call must never wedge the flow run.
pub fn clamp_run_timeout_secs(requested: Option<u64>) -> u64 {
    requested.map(|s| s.clamp(10, 600)).unwrap_or(240)
}

/// Issue #4868 — scale `base_timeout_secs` up for agents whose effective
/// iteration cap exceeds the (until now, universal) global default of 10.
///
/// A `tools_agent`/`code_executor`/etc. node now legitimately runs up to 50
/// iterations (`iteration_policy = "extended"`). At a worst case of
/// ~10s/iteration that's ~500s, comfortably exceeding the 240s
/// `clamp_run_timeout_secs` default — the node would be killed by timeout
/// before it could use its own declared budget. Agents whose effective cap is
/// still at or below the old global default (10) are unaffected and keep the
/// unscaled `base_timeout_secs`. The scaled floor is capped at the existing
/// 600s maximum `clamp_run_timeout_secs` already enforces, so this can only
/// ever raise the effective timeout up to that ceiling, never past it.
pub fn scale_timeout_for_iteration_cap(
    base_timeout_secs: u64,
    effective_iteration_cap: usize,
) -> u64 {
    if effective_iteration_cap > 10 {
        let scaled = (effective_iteration_cap as u64).saturating_mul(12).min(600);
        base_timeout_secs.max(scaled)
    } else {
        base_timeout_secs
    }
}

/// Resolves the actual wall-clock timeout for one agent-node harness turn,
/// combining [`clamp_run_timeout_secs`] and [`scale_timeout_for_iteration_cap`]
/// per the post-merge Codex P2 finding on issue #4868's iteration-cap timeout
/// scaling: **an explicit `timeout_secs` the flow author set on the node must
/// never be scaled up.**
///
/// A node's `timeout_secs` can be an intentional fast-fail/SLA bound (e.g.
/// `timeout_secs: 120` to bound a health-check-style agent call) — scaling
/// that up to match a 50-iteration-cap agent would silently defeat the
/// author's explicit choice. So the iteration-cap scaling only ever widens
/// the *default* (no `timeout_secs` supplied) 240s bound; an explicit value is
/// clamped to `10..=600` (as it always was) and returned as-is.
///
/// `requested_timeout_secs` is the raw `request["timeout_secs"]` (before
/// clamping) so this function can distinguish "caller supplied a value" from
/// "caller supplied nothing" — [`clamp_run_timeout_secs`] alone collapses that
/// distinction into a plain `u64`.
pub fn resolve_run_timeout_secs(
    requested_timeout_secs: Option<u64>,
    effective_iteration_cap: usize,
) -> u64 {
    let base_timeout_secs = clamp_run_timeout_secs(requested_timeout_secs);
    if requested_timeout_secs.is_some() {
        base_timeout_secs
    } else {
        scale_timeout_for_iteration_cap(base_timeout_secs, effective_iteration_cap)
    }
}

/// Inserts `system_prompt` as the first `system` message of a completion
/// `request`, creating the `messages` array (seeded from any `prompt` string)
/// when the request doesn't already carry a non-empty one. Mirrors how
/// `OpenHumanLlm::complete` reads `messages`/`prompt`.
pub fn prepend_system_message(request: &mut Value, system_prompt: &str) {
    let Value::Object(map) = request else {
        return;
    };
    let system_msg = json!({ "role": "system", "content": system_prompt });
    // An empty `messages` falls back to `prompt` downstream: treat it as absent.
    match map
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .filter(|m| !m.is_empty())
    {
        Some(messages) => messages.insert(0, system_msg),
        None => {
            // No `messages`: build one from the `prompt` string (if any).
            let mut messages = vec![system_msg];
            if let Some(prompt) = map.get("prompt").and_then(Value::as_str) {
                messages.push(json!({ "role": "user", "content": prompt }));
            }
            map.insert("messages".to_string(), Value::Array(messages));
        }
    }
}

#[cfg(test)]
#[path = "agent_prompt_shape_tests.rs"]
mod shape_tests;
#[cfg(test)]
#[path = "agent_prompt_tests.rs"]
mod tests;
