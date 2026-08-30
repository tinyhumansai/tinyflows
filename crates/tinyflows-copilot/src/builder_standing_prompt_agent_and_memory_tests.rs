//! Standing-prompt content assertions: reply hygiene, agent-ref selection,
//! and the working-memory read/write split.

use super::contains_normalized;

/// narration, no draft-then-restate, lead with substance. Without these
/// the reasoning-tier model narrates its chain of thought in the visible
/// reply ("let me think… actually wait… let me reconsider") and restates
/// its questions twice in the same message. (The harness already keeps
/// real reasoning blocks out of the visible text — this is the model
/// choosing to narrate in its output, so a prompt rule is the fix.)
#[test]
fn standing_prompt_teaches_reply_hygiene() {
    const STANDING_PROMPT: &str = crate::prompts::WORKFLOW_BUILDER;

    for rule in [
        "finished reply",
        "No deliberation narration",
        "No draft-then-restate",
        "Lead with substance",
    ] {
        assert!(
            contains_normalized(STANDING_PROMPT, rule),
            "standing prompt must teach the reply-hygiene rule `{rule}` — the \
             reply is the finished answer, not a thinking scratchpad (no \
             deliberation narration, no draft-then-restate)"
        );
    }
}

/// Before asking the user for a missing value, the builder must exhaust
/// self-resolution — recall, connections, tool catalog, and (for
/// runtime-only facts like the user's own platform handle) wiring a
/// lookup node — and only ask for genuine preferences, not resolvable
/// facts. This also guards that the existing "zero questions is still
/// the happy path" balance line survives: the rule must not turn into
/// "ask about everything".
#[test]
fn standing_prompt_teaches_resolution_first_self_resolution() {
    const STANDING_PROMPT: &str = crate::prompts::WORKFLOW_BUILDER;

    for rule in [
        "asking is the last resort",
        "Wire a runtime lookup",
        "resolvable facts",
        "genuine preferences",
        "get authenticated user",
        "what you already tried",
        "zero questions is still the happy path",
    ] {
        assert!(
            contains_normalized(STANDING_PROMPT, rule),
            "standing prompt must teach the resolution-first rule `{rule}` — \
             before asking for any missing value, the builder must exhaust \
             self-resolution (recall, connections, tool catalog, runtime \
             lookup) and only ask for genuine preferences, while the \
             zero-questions happy path still holds"
        );
    }
}

/// B37 (Gap 1): the standing prompt must actually teach the builder to
/// reach for a specialist `agent_ref` — ground the id via
/// `list_agent_profiles`, understand that `agent_ref` runs a real agent
/// turn with its own tool loop (not just a persona-flavored completion),
/// and see concrete examples of when a plain agent node isn't enough.
#[test]
fn standing_prompt_teaches_specialist_agent_ref_selection() {
    const STANDING_PROMPT: &str = crate::prompts::WORKFLOW_BUILDER;

    for rule in [
        "list_agent_profiles",
        "Picking a specialist via `agent_ref`",
        "code_executor",
        "researcher",
        "flow_memory_agent",
    ] {
        assert!(
            contains_normalized(STANDING_PROMPT, rule),
            "standing prompt must teach specialist selection via `{rule}` — the \
             builder needs to know it can ground a real agent_ref with \
             list_agent_profiles instead of hallucinating one"
        );
    }
}

/// #5204: `flow_memory_agent` is the general-purpose read-only context/
/// memory route for a flow `agent` node's `agent_ref` — not a fixed list
/// of use cases. The standing prompt must actually teach that generality
/// (not just mention the agent's name once), or the builder keeps
/// reaching for `context_scout`'s narrower structured-bundle niche for
/// requests that don't need a bundle at all.
#[test]
fn standing_prompt_teaches_flow_memory_agent_as_general_context_route() {
    const STANDING_PROMPT: &str = crate::prompts::WORKFLOW_BUILDER;

    assert!(
        contains_normalized(STANDING_PROMPT, "flow_memory_agent"),
        "standing prompt must name `flow_memory_agent`"
    );
    assert!(
        contains_normalized(STANDING_PROMPT, "the PREFERRED general"),
        "standing prompt must teach flow_memory_agent as the PREFERRED general route"
    );
    assert!(
        contains_normalized(STANDING_PROMPT, "for ANY use case, not a fixed list"),
        "standing prompt must state the routing rule is general — ANY use case, not \
         a fixed list of scenarios — or the builder will under-route to flow_memory_agent"
    );
    assert!(
        contains_normalized(STANDING_PROMPT, "narrower niche"),
        "standing prompt must demote context_scout to its narrower structured-bundle \
         niche now that flow_memory_agent is the general route"
    );
    // Regression (Greptile P1 / CodeRabbit): the generic customer-history
    // example must route to flow_memory_agent — routing general history
    // retrieval to context_scout contradicts the rule above and trains the
    // builder to under-route to flow_memory_agent.
    assert!(
        contains_normalized(STANDING_PROMPT, "asked us before\" → `flow_memory_agent`"),
        "the generic customer-history example must route to flow_memory_agent"
    );
    assert!(
        !contains_normalized(STANDING_PROMPT, "asked us before\" → `context_scout`"),
        "the generic customer-history example must NOT route to context_scout — that \
         contradicts flow_memory_agent being the general context/history route"
    );
}

/// The runtime already gives an `agent_ref` step the selected specialist's
/// full persona/model/tool loop/iteration cap (`run_via_harness` in
/// `tinyflows/caps.rs`) — the prompt must say so, not describe it as a
/// future capability.
#[test]
fn standing_prompt_links_agent_ref_to_the_full_tool_loop() {
    const STANDING_PROMPT: &str = crate::prompts::WORKFLOW_BUILDER;

    assert!(
        contains_normalized(STANDING_PROMPT, "specialist")
            && (contains_normalized(STANDING_PROMPT, "tool loop")
                || contains_normalized(STANDING_PROMPT, "full persona")),
        "standing prompt must link agent_ref to the specialist's full tool loop \
         (the harness path), not just a persona/model swap"
    );
}

/// Regression guard: the old `list_agent_profiles` description (and any
/// prompt copy that echoed it) claimed the per-agent tool loop was "a
/// follow-up" and that a step "still gets tools from the node's own
/// inline `tools` list for now". That's false — `run_via_harness` already
/// gives an `agent_ref` step its selected specialist's real tool loop —
/// and the stale wording actively discouraged using `agent_ref` at all.
#[test]
fn standing_prompt_has_no_stale_agent_ref_followup_language() {
    const STANDING_PROMPT: &str = crate::prompts::WORKFLOW_BUILDER;

    for banned in [
        "is a follow-up",
        "for now",
        "still gets tools from the node's own",
    ] {
        assert!(
            !contains_normalized(STANDING_PROMPT, banned),
            "standing prompt must not carry the stale agent_ref-tool-loop \
             phrasing `{banned}` — the harness path already gives agent_ref \
             its full tool loop"
        );
    }
}

/// Guard against over-fragmentation: the minimal-graph rule (don't chain
/// agents doing the same kind of work) must survive alongside the new
/// specialist guidance (do pick a specialist when the step needs tools
/// the plain agent lacks) — neither should crowd the other out.
#[test]
fn standing_prompt_keeps_minimal_graph_warning_alongside_specialist_guidance() {
    const STANDING_PROMPT: &str = crate::prompts::WORKFLOW_BUILDER;

    assert!(
        contains_normalized(STANDING_PROMPT, "minimal viable graph"),
        "standing prompt must still warn to prefer the minimal viable graph"
    );
    assert!(
        contains_normalized(STANDING_PROMPT, "3–6 nodes")
            || contains_normalized(STANDING_PROMPT, "3-6 nodes"),
        "standing prompt must still carry the 3-6 node sizing guidance"
    );
    assert!(
        contains_normalized(STANDING_PROMPT, "SAME kind of work"),
        "standing prompt must still warn against chaining agents doing the \
         same kind of work, even after adding specialist-selection guidance"
    );
}

/// Regression guard for the shipped prompt bug this test was added with:
/// the standing prompt used to claim an `agent` node "can also **read and
/// write the user's memory at run time**". Both halves were false. A plain
/// `agent` node is a single completion through the host's LLM capability — no
/// tool loop, so it can neither read nor write
/// memory. Told otherwise, the builder authored a plain agent node
/// prompted to "recall the user's preference", and the model FABRICATED
/// one: the step silently invented context instead of failing, which is
/// strictly worse than not working. The banned strings below are the exact
/// wording that produced that, so it can never be reintroduced verbatim.
#[test]
fn standing_prompt_does_not_claim_plain_agent_nodes_reach_memory() {
    const STANDING_PROMPT: &str = crate::prompts::WORKFLOW_BUILDER;

    for banned in [
        "read and write the user's\n   memory at run time",
        "wire\n   an `agent` node that uses memory",
    ] {
        assert!(
            !contains_normalized(STANDING_PROMPT, banned),
            "standing prompt must not tell the builder a plain `agent` node can \
             reach memory ({banned:?}) — it has no tool loop, so the model \
             fabricates the recalled value instead of looking it up"
        );
    }

    assert!(
        contains_normalized(
            STANDING_PROMPT,
            "A plain `agent` node has NO\n   memory access"
        ),
        "standing prompt must state outright that a plain agent node has no \
         memory access, so the builder never authors a no-op recall step"
    );
}

/// The four mechanisms that DO reach memory from inside a running flow
/// must all be taught, with the correct binding path for the
/// deterministic `tool_call` one. A host-native tool result is host-defined,
/// so its contract supplies the downstream binding path rather than the shared
/// prompt assuming a vendor envelope. #5204 added `flow_memory_agent` as
/// the PREFERRED general route alongside the deterministic `tool_call`
/// reads and `context_scout`'s narrower niche; the memory-node feature
/// (issue #5226) then added the `memory` node itself as the preferred
/// choice specifically for a non-reasoning node (`condition`/`switch`)
/// that needs to branch on a recalled value.
#[test]
fn standing_prompt_teaches_the_four_working_memory_read_paths() {
    const STANDING_PROMPT: &str = crate::prompts::WORKFLOW_BUILDER;

    for rule in [
        "A `memory` node",
        "host-native memory slug",
        "inspect the tool contract",
        "flow_memory_agent",
        "context_scout",
    ] {
        assert!(
            contains_normalized(STANDING_PROMPT, rule),
            "standing prompt must teach `{rule}` — it is one of the four \
             mechanisms that actually read memory at flow run time, or the \
             binding path needed to consume one"
        );
    }
}

/// Flows run on trigger data a third party can influence (an inbound
/// email, a webhook payload), so writing to the user's PERSONAL memory is
/// deliberately never offered — that guarantee must survive the
/// memory-node feature (issue #5226) verbatim. `agent_memory` is NOT an
/// escape hatch here despite being a registered, `read_only` builtin: its
/// `memory_tree` tool inherits the trait-default `PermissionLevel::ReadOnly`
/// while dispatching an `ingest_document` WRITE mode, so it survives the
/// read-only tool filter in `session/builder/factory.rs` (which consults
/// the argless `permission_level()`). Steering the builder there would
/// hand prompt-injected trigger content a memory-write foothold — exactly
/// the hole `context_scout`'s own agent.toml documents refusing.
///
/// What DID change with #5226: a flow can now write its OWN private,
/// flow-scoped memory (`memory` node, `scope: "flow"`) — the prompt must
/// teach that too, with the "remember after the action, not before" rule,
/// so the builder stops telling users memory writes are unavailable
/// entirely and instead reaches for the real mechanism.
#[test]
fn standing_prompt_states_flows_cannot_write_user_memory_but_can_write_flow_memory() {
    const STANDING_PROMPT: &str = crate::prompts::WORKFLOW_BUILDER;

    assert!(
        contains_normalized(STANDING_PROMPT, "can never WRITE the user's memory"),
        "standing prompt must state plainly that a workflow cannot write the \
         user's PERSONAL memory, so the builder never targets scope \"user\" \
         on a remember/forget memory node"
    );
    assert!(
        !contains_normalized(STANDING_PROMPT, "agent_memory"),
        "standing prompt must not steer the builder to `agent_memory` as a \
         flow agent_ref: its `memory_tree` tool declares ReadOnly but exposes \
         an ingest_document write mode, so it would give prompt-injectable \
         trigger data a memory-write path"
    );
    assert!(
        contains_normalized(STANDING_PROMPT, "scope: \"flow\""),
        "standing prompt must teach that a workflow CAN write its own \
         flow-scoped memory via a `memory` node (scope: \"flow\") — this is \
         the real mechanism for a flow that \"remembers\" across runs, \
         replacing the old blanket \"memory writes are not available\" advice"
    );
    assert!(
        contains_normalized(
            STANDING_PROMPT,
            "Always place the `remember` AFTER the real action"
        ),
        "standing prompt must teach commit-on-success ordering: remember AFTER \
         the action it's recording, never before, so a failed action doesn't \
         get silently marked done"
    );
}
