//! Standing-prompt content assertions: the plain-language / read-only-memory framing.

use super::contains_normalized;

/// The standing archetype (`prompt.md`, the always-loaded system prompt —
/// as opposed to the per-turn directives rendered above) carries the same
/// B27 banned-phrase regression, plus positive coverage for the plain-
/// language style rule and the read-only memory grounding tool added
/// alongside it. Guards against reintroducing jargon-leaking or
/// phantom-review-card language, and against silently losing the
/// `memory_recall` guidance if the prompt is ever rewritten.
#[test]
fn standing_prompt_teaches_plain_language_and_readonly_memory() {
    const STANDING_PROMPT: &str = crate::prompts::WORKFLOW_BUILDER;

    // Negative (B27): the phantom "review card" phrasing must never
    // reappear in the standing prompt either.
    for banned in ["review card", "Accept the proposal explicitly"] {
        assert!(
            !contains_normalized(STANDING_PROMPT, banned),
            "standing prompt must not carry phantom review-card phrasing `{banned}` (B27)"
        );
    }

    // Positive: the anti-jargon Style rule — replies must stay in plain
    // language, never leak response_format/schema/expression internals.
    assert!(
        contains_normalized(STANDING_PROMPT, "Speak to a non-technical user"),
        "standing prompt must teach the anti-jargon Style rule"
    );

    // Positive: read-only memory grounding via the raw `memory_recall`
    // tool (no `memory_store` — see the agent.toml regression test).
    assert!(
        contains_normalized(STANDING_PROMPT, "memory_recall"),
        "standing prompt must teach the builder to ground itself with memory_recall"
    );

    // Positive: the prompt must state the read-only contract explicitly —
    // not just mention the tool name — so a future edit can't silently
    // drop the "can't change their memory" guarantee this agent's tool
    // scope depends on (no `memory_store` in agent.toml).
    assert!(
        contains_normalized(STANDING_PROMPT, "Read-only — you can't change their memory"),
        "standing prompt must state the memory read-only guarantee, not just mention memory_recall"
    );

    // Negative (contract accuracy, issue #6): `create_workflow` and
    // `duplicate_flow` are on this agent's belt (see agent.toml's `named`
    // tool list), so the prompt must never claim the agent can't create a
    // flow at all — only that it can't enable/run one unattended.
    for banned in [
        "create a new flow, or enable/disable one",
        "It cannot create flows,",
    ] {
        assert!(
            !contains_normalized(STANDING_PROMPT, banned),
            "standing prompt must not carry the stale \"can never create a flow\" claim \
             `{banned}` — create_workflow/duplicate_flow are on the belt (issue #6)"
        );
    }

    // Positive: the accurate contract — the agent CAN create a flow, but
    // every flow it creates is always born disabled.
    assert!(
        contains_normalized(STANDING_PROMPT, "create_workflow")
            && contains_normalized(STANDING_PROMPT, "born"),
        "standing prompt must accurately teach that create_workflow exists and that \
         created flows are always born disabled (issue #6)"
    );

    // Positive (Bld §4): run guidance is capability-conditional. `run_flow`
    // (and resume/cancel) are hidden on the `flows_build` path, so the
    // prompt must NOT unconditionally claim the builder can run a flow —
    // it must first check whether the tool is on its belt and, when it is
    // not, point the user to the Workflows UI Run control instead of
    // offering-then-refusing (the confusing "want me to run it?" → "I
    // don't have access" behavior).
    assert!(
        contains_normalized(STANDING_PROMPT, "only if the tool is on your belt")
            && contains_normalized(STANDING_PROMPT, "never offer to run the flow")
            && contains_normalized(STANDING_PROMPT, "Workflows UI"),
        "standing prompt must make run_flow capability-conditional: never offer to run \
         when the tool is off the belt, and point the user to the Workflows UI Run \
         control instead (Bld §4 offer-then-refuse)"
    );

    // Negative: the pre-fix heading ("ask first!") asserted run_flow was
    // simply a confirm-before-use tool, with no capability check at all —
    // it must not reappear (that's the exact offer-then-refuse regression
    // Bld §4 closed).
    assert!(
        !contains_normalized(STANDING_PROMPT, "`run_flow` (ask first!)"),
        "standing prompt must not regress to the pre-Bld-§4 unconditional \
         \"ask first!\" run_flow heading"
    );

    // Positive: the run_flow section must explicitly gate the real-run
    // instructions behind the capability check, not just mention the
    // check somewhere else in the doc — bind the assertion to the two
    // halves of the actual contract (off-belt fallback, on-belt usage).
    assert!(
        contains_normalized(
            STANDING_PROMPT,
            "If you do **not** have a `run_flow` tool, never offer to run the flow"
        ),
        "standing prompt must state the off-belt fallback as a direct consequence \
         of the capability check, not a generic nearby mention"
    );
    assert!(
        contains_normalized(
            STANDING_PROMPT,
            "If you **do** have `run_flow`: once the user has **saved** a flow"
        ),
        "standing prompt must gate the on-belt run_flow usage behind the same \
         capability check"
    );

    // Positive (CodeRabbit follow-up on Bld §4): `resume_flow_run` /
    // `cancel_flow_run` get the identical capability-conditional
    // treatment as `run_flow` — both are hidden alongside it on the
    // `flows_build` path (`FLOWS_BUILD_HIDDEN_TOOLS`), so a fix that only
    // gated `run_flow` while leaving these two unconditional would
    // reopen the same offer-then-refuse bug one hop later.
    assert!(
        contains_normalized(
            STANDING_PROMPT,
            "those tools are on your belt** — `resume_flow_run` (approval-gated) or"
        ),
        "standing prompt must gate resume_flow_run/cancel_flow_run behind the \
         same on-your-belt capability check as run_flow"
    );
    assert!(
        contains_normalized(STANDING_PROMPT, "(if they're not available, point the"),
        "standing prompt must state the resume/cancel off-belt fallback condition"
    );
    assert!(
        contains_normalized(
            STANDING_PROMPT,
            "user to the runs list in the Workflows UI instead of offering)."
        ),
        "standing prompt must point resume/cancel's off-belt fallback to the \
         Workflows UI runs list, matching run_flow's UI fallback pattern"
    );

    // Negative: the pre-fix wording offered resume/cancel unconditionally
    // right after `edit_workflow`, with no capability check in between —
    // must not reappear.
    assert!(
        !contains_normalized(
            STANDING_PROMPT,
            "patch with `edit_workflow`; `resume_flow_run`"
        ),
        "standing prompt must not regress to the pre-fix unconditional \
         resume_flow_run/cancel_flow_run offer"
    );

    // Positive: self-DM resolution — the prompt must teach the builder to
    // wire "DM me" onto the connection's own `platform_user_id`, not a
    // public channel (the #general/#team-product fallback bug).
    assert!(
        contains_normalized(STANDING_PROMPT, "platform_user_id"),
        "standing prompt must teach that list_flow_connections surfaces \
         platform_user_id for self-DM resolution"
    );
    assert!(
        contains_normalized(STANDING_PROMPT, "DM me"),
        "standing prompt must keep the \"DM me\" self-target guidance"
    );
    assert!(
        contains_normalized(
            STANDING_PROMPT,
            "Never default a personal request to a public channel"
        ),
        "standing prompt must explicitly forbid falling back to a public \
         channel (e.g. #general/#team-product) for a personal \"DM me\" request"
    );

    // Positive: assert the *complete* wiring instruction, not just the
    // presence of the `platform_user_id` keyword — a regression could
    // drop the actual "pass it as `channel`" directive while leaving the
    // word `platform_user_id` elsewhere in the prompt and still pass the
    // looser check above.
    assert!(
        contains_normalized(
            STANDING_PROMPT,
            "that id verbatim as the `channel` arg on `SLACK_SEND_MESSAGE`"
        ),
        "standing prompt must explicitly instruct passing `platform_user_id` \
         verbatim as the `channel` arg on `SLACK_SEND_MESSAGE` — not just \
         mention the field name"
    );

    // Positive: the null-`platform_user_id` fallback (ask the user for
    // their member id in one question) must survive too — this is the
    // other half of the self-DM contract and must not be silently lost.
    assert!(
        contains_normalized(STANDING_PROMPT, "Only if `platform_user_id` is null")
            && contains_normalized(STANDING_PROMPT, "ask the user for their member id"),
        "standing prompt must preserve the null-`platform_user_id` fallback: \
         ask the user for their member id in one question rather than \
         guessing a channel"
    );

    // Positive: non-owner DM resolution — the prompt must teach the
    // builder to resolve a NAMED recipient who is NOT the connected
    // owner via a lookup node, not just the owner's own
    // `platform_user_id`. This guidance must be PLATFORM-AGNOSTIC (no
    // toolkit-specific slug hardcoded) — the same shape applies to
    // Slack, Discord, Telegram, or any other messaging toolkit.
    assert!(
        contains_normalized(STANDING_PROMPT, "is NOT the connected"),
        "standing prompt must teach the non-owner DM case explicitly"
    );
    assert!(
        contains_normalized(STANDING_PROMPT, "platform-agnostic"),
        "standing prompt must state the non-owner DM guidance is \
         platform-agnostic, not tied to one toolkit"
    );
    assert!(
        contains_normalized(STANDING_PROMPT, "search_tool_catalog { query, toolkit }"),
        "standing prompt must teach resolving the lookup action via \
         search_tool_catalog scoped to the TARGET toolkit, rather than \
         hardcoding one platform's slug"
    );
    assert!(
        contains_normalized(STANDING_PROMPT, "tool_call` node upstream of the send"),
        "standing prompt must teach wiring the lookup as a tool_call \
         node upstream of the send node"
    );
    assert!(
        contains_normalized(STANDING_PROMPT, "resolves to exactly one match"),
        "standing prompt must require a name search to resolve to \
         exactly one match before binding it without asking"
    );
    assert!(
        contains_normalized(STANDING_PROMPT, "ask the user to confirm which person"),
        "standing prompt must preserve the safety rule: never message an \
         unverified same-name match, ask instead when ambiguous"
    );
    assert!(
        contains_normalized(STANDING_PROMPT, "Check the send action")
            && contains_normalized(STANDING_PROMPT, "open conversation"),
        "standing prompt must teach checking the send tool's own contract \
         for a required open-conversation step, handled generally via the \
         contract rather than a single-platform special case"
    );

    // Negative: none of the non-owner DM guidance may hardcode a
    // toolkit-specific action slug or arg name — the reviewer flagged an
    // earlier draft of this guidance as Slack-only, which violates the
    // platform-agnostic rule.
    for banned in [
        "SLACK_FIND_USERS",
        "SLACK_LIST_ALL_USERS",
        "config.args.email",
        "exact_match",
    ] {
        assert!(
            !contains_normalized(STANDING_PROMPT, banned),
            "standing prompt's non-owner DM guidance must not hardcode \
             the platform-specific `{banned}` — it must stay \
             platform-agnostic (any messaging toolkit)"
        );
    }
}
