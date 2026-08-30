//! The `agent` node: an LLM agent turn with optional sub-ports.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;

use crate::data::Item;
use crate::error::Result;
use crate::nodes::integration::schema;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};
use crate::transcript::TranscriptEntry;

/// Collects every turn's transcript across one node activation.
///
/// A `per_item` agent node runs one turn per input item while the step it
/// reports carries a single transcript, so the entries have to accumulate
/// somewhere shared. Shared rather than returned so `map_items`' closure
/// signature stays as it is.
///
/// Keyed by **item index** rather than appended flat, because `per_item` with
/// `concurrency > 1` completes turns in whatever order they finish while
/// `map_items` deliberately restores its outputs to input order. Appending on
/// completion would make one run's transcript interleave differently from the
/// next for identical input; draining in key order makes it match the items it
/// describes. `None` is the single-turn case, which sorts first and is alone.
type TranscriptSink = Arc<Mutex<BTreeMap<Option<usize>, Vec<TranscriptEntry>>>>;

/// Keeps `entries` for the settled step, under the item they belong to.
///
/// Not reported live: they ride the outcome the harness returns, so by the time
/// this runs the turn is over. See `ExecutionStep::transcript`.
fn record_transcript(
    ctx: &NodeContext<'_>,
    sink: &TranscriptSink,
    item_index: Option<usize>,
    entries: Vec<TranscriptEntry>,
) {
    if entries.is_empty() {
        return;
    }
    // A poisoned lock must not fail the turn: the agent already ran and its
    // answer is good. Losing the transcript degrades the run history, which is
    // strictly better than discarding the work.
    match sink.lock() {
        Ok(mut held) => held.entry(item_index).or_default().extend(entries),
        Err(err) => tracing::warn!(
            node = %ctx.node.id,
            %err,
            "agent node: transcript sink poisoned; dropping this turn's entries"
        ),
    }
}

/// Takes everything the sink holds, in item order, leaving it empty.
fn drain(sink: &TranscriptSink) -> Vec<TranscriptEntry> {
    sink.lock()
        .map(|mut held| std::mem::take(&mut *held).into_values().flatten().collect())
        .unwrap_or_default()
}

/// Runs an LLM agent turn, optionally composed with **sub-ports** that attach an
/// output parser and tools to the bare completion.
///
/// The node config is the completion request handed to the injected
/// [`LlmProvider`](crate::caps::LlmProvider). On top of that, two sub-ports are
/// wired (config-embedded, so a plain agent node with just a prompt still works
/// unchanged):
///
/// - **tool sub-port** (`config.tools`): the available tools are surfaced to the
///   model in the request. If the model's response elects to call one of the
///   *offered* tools — a `tool_call: { slug, args?, connection_ref? }` object in
///   the response — the agent invokes it once via
///   [`ToolInvoker`](crate::caps::ToolInvoker) and attaches the result under
///   `tool_result`. This is a **single hop** (no unbounded agent loop) — a full
///   multi-turn tool-use loop is a documented follow-up.
/// - **output-parser sub-port** (`config.output_parser`): after the completion
///   (and any tool hop), the resulting value is validated/repaired against
///   `config.output_parser.schema` using the shared [`schema`] routine
///   (validate → one LLM auto-fix → re-validate), honoring
///   `config.output_parser.auto_fix` (default `true`).
///
/// **Agent-kind selection** (`config.agent_ref`): when set and the host wired the
/// optional [`AgentRunner`](crate::caps::AgentRunner) capability, the node runs
/// that named agent to completion instead of issuing a bare completion.
/// `agent_ref` resolves against the graph's own
/// [`agents`](crate::model::WorkflowGraph::agents) registry first, then the
/// harness's, then passes through as a bare id. The resulting
/// [`AgentDefinition`](crate::model::AgentDefinition) is merged with the node's
/// overrides — **narrowing only** — and assembled into a typed
/// [`AgentRunRequest`](crate::caps::AgentRunRequest) carrying the resolved
/// instructions, model, provider, working directory, context blocks, tool
/// descriptors, limits, and metadata. See
/// [`agent_request`](super::agent_request) for the merge rules.
///
/// The inline tool sub-port is skipped on that path (the agent owns its own tool
/// loop, and re-invoking a tool it already called would duplicate the side
/// effect); the output-parser sub-port still applies. `agent_ref` is read from
/// trusted node config, never from model output.
///
/// With no `agent_ref` (or no `AgentRunner`) the node falls back to
/// [`LlmProvider`](crate::caps::LlmProvider) exactly as it always has — the one
/// addition being that a declared `context` is still resolved into blocks, so an
/// author's context is not silently dropped on a host without a harness.
///
/// **Output.** The agent-kind path emits `{ json, text, raw, meta }`, where
/// `meta.stop` is `"finished"` or `"limit_stop"` — so a downstream `condition`
/// can branch on whether the agent actually reached an answer. A `limit_stop`
/// payload is partial and skips the output parser. The degraded path emits the
/// original three-key envelope unchanged. A harness reporting
/// [`Paused`](crate::caps::StopReason::Paused) fails the node: resuming a paused
/// agent is not supported yet, and emitting a half-run agent's output as if it
/// were an answer is the failure this refuses to make.
///
/// Sub-ports **not** yet wired (documented follow-ups): a `chat_model` sub-port
/// (attached model selection beyond what the request already carries) and a
/// `memory` sub-port (conversation memory injected into the request / persisted
/// across turns). Those require attached-node wiring and/or `StateStore` plumbing
/// and are deliberately left out rather than stubbed.
#[derive(Debug, Default, Clone)]
pub struct AgentNode;

#[async_trait]
impl NodeExecutor for AgentNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        // Execution mode (default `once`): an agent turn is usually batch-level,
        // but `per_item` maps it over the input (one turn per item, config
        // re-resolved against each) for row-wise agent processing.
        let per_item =
            crate::nodes::execution_mode(&ctx.node.config, crate::nodes::ExecutionMode::Once)
                == crate::nodes::ExecutionMode::PerItem
                && !ctx.input.is_empty();

        // One accumulator for the whole node activation, because a `per_item`
        // agent node runs many turns and the step carries one transcript. Shared
        // rather than returned, so `map_items`' closure signature is untouched.
        let transcript: TranscriptSink = Arc::new(Mutex::new(BTreeMap::new()));

        if per_item {
            // Fan out: `config.concurrency` decides how many turns run at once
            // (default 1 — sequential, as this node has always behaved), and
            // `config.on_item_error` what a failing turn does to the batch.
            let opts = crate::nodes::map::map_options(&ctx.node.config, &ctx.node.id, ctx.run);
            let ctx = &ctx;
            let transcript = &transcript;
            let (items, diagnostics) = crate::nodes::map::map_items(
                ctx.input.len(),
                &ctx.node.id,
                ctx.observer,
                opts,
                move |index| async move {
                    let item_json = ctx.input[index].json.clone();
                    let (cfg, diags) =
                        crate::nodes::resolve_config_traced_for_item(ctx, item_json.clone());
                    // The same scope the config was resolved against, so an
                    // agent definition's own `=`-expressions see this item.
                    let scope = crate::nodes::expr_scope_for(ctx, item_json);
                    let item = run_turn_indexed(ctx, &cfg, &scope, Some(index), transcript).await?;
                    Ok((item, diags))
                },
            )
            .await?;
            return Ok(NodeOutput::main(items)
                .with_diagnostics(diagnostics)
                .with_transcript(drain(transcript)));
        }

        // Single turn against the first-item scope (or empty input).
        let (cfg, diagnostics) = crate::nodes::resolve_config_traced(&ctx);
        let scope = crate::nodes::expr_scope(&ctx);
        let item = run_turn(&ctx, &cfg, &scope, &transcript).await?;
        Ok(NodeOutput::main(vec![item])
            .with_diagnostics(diagnostics)
            .with_transcript(drain(&transcript)))
    }
}

/// Runs one agent turn against an already-resolved `cfg`: the completion (or
/// registered agent kind), the optional tool sub-port, the optional
/// output-parser sub-port, and finally the stable `{ json, text, raw }`
/// envelope. Returns the emitted [`Item`] (without pairing — the caller sets it).
async fn run_turn(
    ctx: &NodeContext<'_>,
    cfg: &Value,
    scope: &Value,
    transcript: &TranscriptSink,
) -> Result<Item> {
    run_turn_indexed(ctx, cfg, scope, None, transcript).await
}

/// [`run_turn`], told which input item it is running for under `per_item`
/// execution, so the harness can attribute the run.
async fn run_turn_indexed(
    ctx: &NodeContext<'_>,
    cfg: &Value,
    scope: &Value,
    item_index: Option<usize>,
    transcript: &TranscriptSink,
) -> Result<Item> {
    let conn = cfg.get("connection_ref").and_then(Value::as_str);

    // Agent-kind selection: a trusted `agent_ref` in config routes this node
    // to a host-registered agent (its own tools/model/sandbox) via the
    // optional `AgentRunner` capability, instead of a bare completion. The ref
    // comes from resolved node config — never from model output — so it can't
    // be steered by prompt injection. Falls back to `LlmProvider` when no
    // `agent_ref` is set or the host wired no agent registry.
    let agent_ref = super::agent_request::agent_ref_of(cfg);
    let via_agent_kind = agent_ref.is_some() && ctx.caps.agent.is_some();

    if let (Some(agent_ref), Some(runner)) = (agent_ref, ctx.caps.agent.as_ref()) {
        tracing::debug!(agent_ref, "agent node: running registered agent kind");
        let request =
            super::agent_request::assemble(ctx, cfg, agent_ref, scope, item_index).await?;
        let outcome = runner.run(request).await?;
        return finish_agent_run(ctx, cfg, conn, agent_ref, outcome, transcript, item_index).await;
    }

    // Degraded path: no agent kind selected, or no harness wired. The node
    // config *is* the completion request, exactly as it has always been — when
    // a `tools` sub-port is configured its descriptors ride along so the model
    // can elect to call one.
    //
    // The one addition: when the author declared `context`, its sources are
    // resolved and the key is replaced with the resulting blocks, so declared
    // context still reaches the model on a host with no `AgentRunner`. Nodes
    // that declare no context are untouched, so an existing graph's request is
    // byte-identical to what it has always been.
    let mut request = match cfg.get("context") {
        Some(raw) if !raw.is_null() => {
            let sources: Vec<crate::model::ContextSource> = serde_json::from_value(raw.clone())
                .map_err(|e| {
                    crate::error::EngineError::Capability(format!(
                        "agent node {}: invalid `context`: {e}",
                        ctx.node.id
                    ))
                })?;
            let identity = super::agent_request::identity_of(ctx, item_index);
            let blocks = super::agent_request::resolve_context(ctx, &sources, &identity).await?;
            let mut request = cfg.clone();
            request["context"] = serde_json::to_value(blocks).unwrap_or(Value::Null);
            request
        }
        _ => cfg.clone(),
    };
    // A declared working directory is checked on this path too. The bare
    // completion has nowhere to *run*, so the resolved value is only normalised
    // into the request for a provider that forwards it — but an author who
    // pointed a step at a directory that escapes the workspace or is not there
    // is told so here rather than discovering it the next time a harness is
    // wired. Both spellings are set to the resolved path so a provider reading
    // either sees the same directory.
    if let Some(raw) = super::agent_request::declared_working_dir(cfg, &ctx.node.id)? {
        let key = if cfg.get("cwd").is_some() {
            "cwd"
        } else {
            "working_dir"
        };
        let resolved = super::agent_request::resolve_working_dir(ctx, &raw, key).await?;
        if let Value::Object(map) = &mut request {
            map.insert("cwd".to_string(), Value::from(resolved.clone()));
            map.insert("working_dir".to_string(), Value::from(resolved));
        }
    }
    let response = ctx.caps.llm.complete(request, conn).await?;

    // `text`/`raw` are derived from the untouched completion; `value` is the
    // structured payload we thread the sub-ports (tool hop, output parser)
    // through before it becomes the envelope's `json`.
    let text = response.as_str().map(str::to_string).or_else(|| {
        response
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let raw = response.clone();

    // Tool sub-port (single hop): honor a `tool_call` the model returned, but
    // only for a tool that was actually offered in `config.tools`. Skipped
    // when a registered agent kind ran — that agent drives its own tool loop.
    let mut value = response;
    let model_tool_call = if via_agent_kind {
        None
    } else {
        value.get("tool_call").cloned()
    };
    if let Some(tool_call) = model_tool_call {
        if let Some(slug) = tool_call.get("slug").and_then(Value::as_str) {
            // Only a tool actually offered in `config.tools` may be invoked;
            // keep the matched descriptor so its trusted `connection_ref` wins.
            let offered_tool = cfg
                .get("tools")
                .and_then(Value::as_array)
                .and_then(|tools| {
                    tools
                        .iter()
                        .find(|t| t.get("slug").and_then(Value::as_str) == Some(slug))
                });
            if let Some(offered_tool) = offered_tool {
                tracing::debug!(slug, "agent tool sub-port: invoking model-elected tool");
                let args = tool_call.get("args").cloned().unwrap_or(Value::Null);
                // Credentials come from trusted config only: the offered tool
                // descriptor's `connection_ref`, else the node's. The model's
                // `tool_call.connection_ref` is deliberately NOT trusted — a
                // prompt-injection could otherwise elect an arbitrary host
                // credential id for the call.
                let tool_conn = offered_tool
                    .get("connection_ref")
                    .and_then(Value::as_str)
                    .or(conn);
                let result = ctx.caps.tools.invoke(slug, args, tool_conn).await?;
                if let Value::Object(map) = &mut value {
                    map.insert("tool_result".to_string(), result);
                }
            } else {
                tracing::warn!(
                    slug,
                    "agent tool sub-port: model elected an un-offered tool; ignoring"
                );
            }
        }
    }

    // Output-parser sub-port: validate/repair the agent output against a schema.
    if let Some(parser) = cfg.get("output_parser").filter(|p| !p.is_null()) {
        if let Some(parser_schema) = parser.get("schema").filter(|s| !s.is_null()) {
            let auto_fix = parser
                .get("auto_fix")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let parser_conn = parser
                .get("connection_ref")
                .and_then(Value::as_str)
                .or(conn);
            value = schema::parse_and_validate(
                value,
                parser_schema,
                auto_fix,
                &ctx.caps.llm,
                parser_conn,
            )
            .await?;
        }
    }

    // Emit the stable envelope: `json` is the structured (tool/parser-threaded)
    // value, `text` the model's prose, `raw` the original completion — so a
    // downstream node reads `=item.text` / `=item.json.<field>` regardless of
    // which sub-ports fired. See [`super::envelope`].
    let json = match value {
        Value::Object(_) | Value::Array(_) => value,
        _ => Value::Null,
    };
    Ok(Item::new(super::envelope::from_parts(json, text, raw)))
}

/// Turns a harness's [`AgentRunOutcome`] into the emitted [`Item`]: honor the
/// stop reason, apply the output-parser sub-port, and publish the envelope.
///
/// The stop reason is read **before** the payload, which is the point of having
/// one. A bare value cannot distinguish an agent that answered from one that ran
/// out of budget one step short, and the workflow marches downstream with a
/// partial answer either way.
async fn finish_agent_run(
    ctx: &NodeContext<'_>,
    cfg: &Value,
    conn: Option<&str>,
    agent_ref: &str,
    outcome: crate::caps::AgentRunOutcome,
    transcript: &TranscriptSink,
    item_index: Option<usize>,
) -> Result<Item> {
    use crate::caps::StopReason;

    // Before the stop reason is judged, so a run that paused or hit a limit
    // still explains what it managed to do — that is exactly the run whose
    // transcript is worth reading.
    record_transcript(ctx, transcript, item_index, outcome.transcript);

    let mut meta = serde_json::json!({ "stop": outcome.stop.as_str(), "agent_ref": agent_ref });

    match &outcome.stop {
        StopReason::Finished => {}
        StopReason::LimitStop { limit } => {
            tracing::warn!(
                node = %ctx.node.id,
                agent_ref,
                limit = %limit,
                "agent node: the agent stopped on a limit; its output is partial"
            );
            meta["limit"] = Value::from(limit.clone());
        }
        StopReason::Paused { reason, .. } => {
            // A pause is resumable, not finished — and the engine cannot yet
            // route one into its checkpoint/resume machinery. Failing loudly
            // beats emitting a half-run agent's output as if it were an answer;
            // see `StopReason::Paused`.
            return Err(crate::error::EngineError::Capability(format!(
                "agent node {}: agent `{agent_ref}` paused ({reason}); resuming a paused agent \
                 is not supported yet, so the run cannot continue",
                ctx.node.id
            )));
        }
    }

    if let Some(usage) = outcome.usage {
        meta["usage"] = serde_json::to_value(usage).unwrap_or(Value::Null);
    }

    // Output-parser sub-port. Deliberately skipped on a limit stop: validating
    // a knowingly partial payload against the author's schema either fails for
    // the wrong reason or, with `auto_fix`, spends a model call inventing the
    // missing half.
    let mut value = outcome.json;
    if matches!(outcome.stop, StopReason::Finished)
        && let Some(parser) = cfg.get("output_parser").filter(|p| !p.is_null())
        && let Some(parser_schema) = parser.get("schema").filter(|s| !s.is_null())
    {
        let auto_fix = parser
            .get("auto_fix")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let parser_conn = parser
            .get("connection_ref")
            .and_then(Value::as_str)
            .or(conn);
        value =
            schema::parse_and_validate(value, parser_schema, auto_fix, &ctx.caps.llm, parser_conn)
                .await?;
    }

    let json = match value {
        Value::Object(_) | Value::Array(_) => value,
        _ => Value::Null,
    };
    Ok(Item::new(super::envelope::from_parts_with_meta(
        json,
        outcome.text,
        outcome.raw,
        meta,
    )))
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;
