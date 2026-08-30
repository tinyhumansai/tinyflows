/// A child workflow's final run state, in the shape the engine records it.
///
/// Raw run-state slots, so items are serialized (`{"json": …}`) — one shape in
/// from the parent's scope projection, which exposes bare payloads. Getting
/// that boundary wrong is precisely what `child_answer` has to survive.
fn child_run_state() -> serde_json::Value {
    json!({
        "run": { "trigger": [], "inputs": { "repo": "acme/thing" } },
        "nodes": {
            // The trigger slot, verbatim from a real run: its payload is the
            // seeded item ARRAY, not an object. Every child has one, and it is
            // what made the first spelling of the projection fail — a
            // fixture whose slots were all objects passed while the real
            // thing resolved the whole prompt to null.
            "start": { "items": [{ "json": [{ "json": {} }] }] },
            "fetch_pr": { "items": [{ "json": {
                "json": { "exit_code": 0, "stdout": "3 files changed" },
                "text": null, "raw": {}
            } }] },
            "verdict": { "items": [{ "json": {
                "json": { "text": "Requesting changes.", "worker": "local" },
                "text": "Requesting changes.",
                "raw": { "text": "Requesting changes." }
            } }] }
        }
    })
}

#[test]
fn an_agents_prose_is_read_from_the_envelopes_text_not_from_inside_its_json() {
    // The regression this file exists to prevent, found in this file's own
    // output: `item.json.text` reads a `text` field inside the STRUCTURED
    // value, which a prose reply has not got, so every `reads` of an agent
    // step rendered "(no output)". Evaluated rather than string-matched —
    // asserting the path spelling is what let the wrong spelling ship.
    let recipe = json!({
        "why": "chain of agents",
        "steps": [
            { "id": "draft", "ask": "Draft it." },
            { "id": "polish", "ask": "Polish the draft.", "reads": ["draft"] }
        ]
    });
    let (graph, _, _) = lower(&recipe, &[]).expect("lowers");
    let prompt = graph.nodes[2].config["prompt"].as_str().expect("prompt");

    let scope = json!({
        "run": {}, "inputs": null, "item": null, "items": [],
        "nodes": { "draft": { "item": {
            "json": "The draft, in prose.",
            "text": "The draft, in prose.",
            "raw": "The draft, in prose."
        } } }
    });
    let rendered = tinyflows::expr::resolve(&json!(prompt), &scope);
    let rendered = rendered.as_str().expect("resolves to a string");
    assert!(
        rendered.contains("The draft, in prose."),
        "an upstream agent's reply must reach the next step: {rendered}"
    );
    assert!(
        !rendered.contains("(no output)"),
        "it rendered the missing-value marker instead: {rendered}"
    );
}

#[test]
fn a_scripts_stdout_is_read_from_inside_its_json_because_that_is_where_it_is() {
    // The asymmetry that made the agent path look right: a shell node's
    // structured value genuinely holds `{exit_code, stdout}`, so this one IS
    // nested. Pinned by evaluation so the two never get "harmonised".
    let (graph, _, _) = lower(&review_recipe(), &[]).expect("lowers");
    let prompt = graph.nodes[2].config["prompt"].as_str().expect("prompt");
    let scope = json!({
        "run": { "inputs": { "repo": "acme/thing" } },
        "inputs": { "repo": "acme/thing" }, "item": null, "items": [],
        "nodes": { "fetch": { "item": {
            "json": { "exit_code": 0, "stdout": "#41 flaky test" },
            "text": null, "raw": {}
        } } }
    });
    let rendered = tinyflows::expr::resolve(&json!(prompt), &scope);
    let rendered = rendered.as_str().expect("resolves to a string");
    assert!(rendered.contains("#41 flaky test"), "{rendered}");
    assert!(
        rendered.contains("acme/thing"),
        "declared inputs too: {rendered}"
    );
}

#[test]
fn an_errand_lowers_to_one_agent_turn_that_validates() {
    let graph = super::errand("how much disk is this directory using").expect("lowers");

    assert!(
        validate_all(&graph).is_empty(),
        "the engine must accept it: {:?}",
        validate_all(&graph)
    );
    // A trigger and exactly one agent node. Anything more means the errand
    // grew a procedure, which is the one thing it is defined as not having.
    assert_eq!(graph.nodes.len(), 2, "{:?}", graph.nodes);
    assert_eq!(graph.nodes[0].kind, NodeKind::Trigger);
    assert_eq!(graph.nodes[1].kind, NodeKind::Agent);
    assert!(
        graph.inputs.is_empty(),
        "an errand declares nothing: it is answered from the goal alone"
    );
}

#[test]
fn an_errands_prompt_actually_carries_the_goal() {
    // Evaluated, not string-matched. The `item.json.text` defect shipped past a
    // reviewer *and* a test because both read the expression instead of running
    // it — a prompt that resolves to nothing looks fine as source.
    let graph = super::errand("  how much disk is this directory using  ").expect("lowers");
    let prompt = graph.nodes[1].config["prompt"]
        .as_str()
        .expect("the agent node carries a prompt");
    let scope = json!({
        "run": { "inputs": {} }, "inputs": {},
        "item": null, "items": [], "nodes": {}
    });
    let rendered = tinyflows::expr::resolve(&json!(prompt), &scope);
    let rendered = rendered.as_str().expect("resolves to a string");
    assert_eq!(
        rendered, "how much disk is this directory using",
        "the goal, trimmed, and nothing else bolted on"
    );
}

#[test]
fn an_errand_is_the_same_lowering_an_authored_ask_gets() {
    // Why `errand` goes through `lower` rather than building two nodes by hand:
    // a second definition of what an `ask` compiles to would drift silently the
    // first time the envelope path changed.
    let errand = super::errand("say something").expect("lowers");
    let (authored, _, _) = lower(
        &json!({
            "why": "one turn of work, no procedure in it",
            "declared": [], "inputs": {},
            "steps": [{ "id": "errand", "ask": "say something" }]
        }),
        &[],
    )
    .expect("lowers");
    assert_eq!(errand.nodes[1].config, authored.nodes[1].config);
}

#[test]
fn every_control_character_in_a_goal_survives_as_a_valid_jq_literal() {
    let scope = json!({ "run": { "inputs": {} }, "inputs": {}, "item": null,
                        "items": [], "nodes": {} });
    let mut broke = Vec::new();
    for code in 0u32..0x20 {
        let ch = char::from_u32(code).expect("control char");
        let goal = format!("disk{ch}usage");
        let Ok(graph) = super::errand(&goal) else {
            broke.push(format!("U+{code:04X}: lowering refused it"));
            continue;
        };
        let prompt = graph.nodes[1].config["prompt"].as_str().expect("prompt");
        let rendered = tinyflows::expr::resolve(&json!(prompt), &scope);
        if rendered.as_str().is_none() {
            broke.push(format!("U+{code:04X}: {rendered:?}"));
        }
    }
    assert!(
        broke.is_empty(),
        "control characters that broke the prompt: {broke:?}"
    );
}
