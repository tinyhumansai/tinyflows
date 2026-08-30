//! The lowering is where authoring mistakes used to live — every test here
//! is a defect class a real episode paid for.

use serde_json::json;
use tinyflows::model::NodeKind;
use tinyflows::validate::validate_all;

use super::lower;

fn review_recipe() -> serde_json::Value {
    json!({
        "why": "fetch then review",
        "declared": [
            { "name": "repo", "description": "owner/name to review", "required": true }
        ],
        "inputs": { "repo": "acme/thing" },
        "steps": [
            { "id": "fetch", "run": "gh issue list --json number,title" },
            { "id": "review", "ask": "Write the verdict report.", "reads": ["fetch"] }
        ]
    })
}

#[test]
fn a_recipe_lowers_to_a_graph_that_validates() {
    let (graph, inputs, why) = lower(&review_recipe(), &[]).expect("lowers");
    assert!(
        validate_all(&graph).is_empty(),
        "a lowered graph must always validate: {:?}",
        validate_all(&graph)
    );
    assert_eq!(graph.nodes.len(), 3, "trigger + two steps");
    assert_eq!(graph.nodes[1].kind, NodeKind::Shell);
    assert_eq!(
        graph.nodes[1].config["source"], "gh issue list --json number,title",
        "the engine's shell node reads config.source — a lowered run step \
         under any other key is born broken, and the model cannot fix it"
    );
    assert_eq!(graph.nodes[2].kind, NodeKind::Agent);
    assert_eq!(inputs["repo"], "acme/thing");
    assert_eq!(why, "fetch then review");
}

#[test]
fn the_generated_prompt_is_one_expression_with_the_right_envelope_paths() {
    let (graph, _, _) = lower(&review_recipe(), &[]).expect("lowers");
    let prompt = graph.nodes[2].config["prompt"].as_str().expect("prompt");
    // The whole string is an expression — the prose-binding failure class
    // cannot occur by construction.
    assert!(prompt.starts_with('='), "{prompt}");
    // A shell upstream is read through `stdout`, the field its kind emits —
    // the exact path a blind author guessed wrong three runs straight.
    assert!(
        prompt.contains(".nodes[\"fetch\"].item.json.stdout"),
        "{prompt}"
    );
    // Declared inputs are attached without the model writing any binding.
    assert!(prompt.contains(".run.inputs[\"repo\"]"), "{prompt}");
    // Absent values surface as markers, not silent nothing.
    assert!(prompt.contains("(no output)"), "{prompt}");
}

#[test]
fn an_agent_upstream_is_read_through_text_not_stdout() {
    let recipe = json!({
        "why": "chain of agents",
        "steps": [
            { "id": "draft", "ask": "Draft it." },
            { "id": "polish", "ask": "Polish the draft.", "reads": ["draft"] }
        ]
    });
    let (graph, _, _) = lower(&recipe, &[]).expect("lowers");
    let prompt = graph.nodes[2].config["prompt"].as_str().expect("prompt");
    assert!(prompt.contains(".nodes[\"draft\"].item.text"), "{prompt}");
}

#[test]
fn quotes_and_newlines_in_an_ask_survive_as_a_valid_jq_literal() {
    let recipe = json!({
        "why": "quoting",
        "steps": [
            { "id": "speak", "ask": "Say \"hello\",\nthen stop." }
        ]
    });
    let (graph, _, _) = lower(&recipe, &[]).expect("lowers");
    let prompt = graph.nodes[1].config["prompt"].as_str().expect("prompt");
    assert!(prompt.contains("\\\"hello\\\""), "{prompt}");
    assert!(prompt.contains("\\n"), "{prompt}");
}

#[test]
fn a_worker_on_an_ask_step_becomes_agent_ref() {
    let recipe = json!({
        "why": "placed work",
        "steps": [
            { "id": "build", "ask": "Build it.", "worker": "ci-box" }
        ]
    });
    let (graph, _, _) = lower(&recipe, &[]).expect("lowers");
    assert_eq!(graph.nodes[1].config["agent_ref"], "ci-box");
}

#[test]
fn every_structural_problem_is_reported_at_once_with_the_fix() {
    let recipe = json!({
        "why": "broken",
        "steps": [
            { "id": "fetch", "run": "true", "reads": ["later"] },
            { "id": "fetch", "ask": "duplicate id" },
            { "id": "confused", "run": "true", "ask": "both" },
            { "id": "empty" }
        ]
    });
    let err = lower(&recipe, &[]).expect_err("refused").to_string();
    for fragment in ["EARLIER", "unique", "so split it", "has none of"] {
        assert!(err.contains(fragment), "missing `{fragment}` in: {err}");
    }
}

#[test]
fn a_declared_value_pasted_into_an_ask_is_refused_with_the_remedy() {
    // Observed on a live host: the author declared `topic` AND wrote
    // "about the topic 'warm caches'" in the ask. The lowering attaches the
    // value anyway, so the paste is redundant now — and poisonous later:
    // selected for a different topic, the prompt carries both, and the keep
    // gate rightly refuses to file the plan. Caught here, the feedback
    // round fixes it before anything runs.
    let recipe = json!({
        "why": "poem",
        "declared": [{ "name": "topic", "description": "", "required": true }],
        "inputs": { "topic": "warm caches" },
        "steps": [
            { "id": "write", "ask": "Write a two-line poem about the topic 'warm caches'." }
        ]
    });
    let err = lower(&recipe, &[]).expect_err("refused").to_string();
    assert!(err.contains("pastes the value"), "{err}");
    assert!(err.contains("attached automatically"), "{err}");

    // The same plan without the paste is exactly what should be written.
    let clean = json!({
        "why": "poem",
        "declared": [{ "name": "topic", "description": "", "required": true }],
        "inputs": { "topic": "warm caches" },
        "steps": [
            { "id": "write", "ask": "Write a two-line poem about the given topic." }
        ]
    });
    lower(&clean, &[]).expect("keepable");
}

#[test]
fn an_undeclared_input_value_in_an_ask_is_not_a_paste() {
    // Undeclared entries never attach to an ask — the author gate trims
    // them — so their values appearing in prose prove nothing about reuse.
    let recipe = json!({
        "why": "poem",
        "inputs": { "stray": "warm caches" },
        "steps": [
            { "id": "write", "ask": "Write a two-line poem about warm caches." }
        ]
    });
    lower(&recipe, &[]).expect("not a paste — nothing declared");
}

#[test]
fn an_indistinct_input_value_in_an_ask_is_not_a_paste() {
    // "on" appears in half of all prose; refusing on it would block
    // perfectly reusable plans. Only distinctive values count.
    let recipe = json!({
        "why": "toggle",
        "declared": [{ "name": "mode", "description": "", "required": true }],
        "inputs": { "mode": "on" },
        "steps": [
            { "id": "flip", "ask": "Turn the feature on if the mode input says so." }
        ]
    });
    lower(&recipe, &[]).expect("not a paste");
}

#[test]
fn a_reply_with_no_steps_says_what_to_return() {
    let err = lower(&json!({ "why": "empty" }), &[])
        .expect_err("refused")
        .to_string();
    assert!(err.contains("at least one step"), "{err}");
}

#[test]
fn the_lowered_shell_config_satisfies_the_engines_own_contract() {
    // Field observation, three attempts of one episode: every run step
    // failed with "shell node missing inline script or script_path" while
    // the author rationally iterated on the only thing the feedback named —
    // a config key it does not write. The lowering emitted `script`; the
    // engine reads `source`. This test asks the ENGINE which required
    // fields its shell contract has and asserts the lowering fills one, so
    // the two cannot drift apart silently again.
    let recipe = json!({
        "why": "fetch",
        "steps": [{ "id": "fetch", "run": "echo hi" }]
    });
    let (graph, _, _) = lower(&recipe, &[]).expect("lowers");
    let shell = tinyflows::catalog::all_contracts()
        .iter()
        .find(|contract| contract.kind == "shell")
        .expect("the engine has a shell contract")
        .clone();
    let required: Vec<&str> = shell
        .config_fields
        .iter()
        .filter(|field| field.required)
        .map(|field| field.name.as_str())
        .collect();
    let config = &graph.nodes[1].config;
    // The shell contract's requirement is one-of (source | script_path), so
    // required may be empty — assert on the actual read keys instead when so.
    if required.is_empty() {
        assert!(
            config.get("source").is_some() || config.get("script_path").is_some(),
            "a lowered run step must fill config.source or config.script_path: {config}"
        );
    } else {
        assert!(
            required.iter().any(|name| config.get(*name).is_some()),
            "the lowering fills none of the engine's required shell fields \
             {required:?}: {config}"
        );
    }
}

#[test]
fn ids_are_sanitized_into_engine_and_jq_safe_names() {
    let recipe = json!({
        "why": "messy ids",
        "steps": [
            { "id": "  Fetch-Issues! ", "run": "true" },
            { "id": "review", "ask": "Review.", "reads": ["Fetch-Issues!"] }
        ]
    });
    let (graph, _, _) = lower(&recipe, &[]).expect("lowers");
    assert_eq!(graph.nodes[1].id, "fetch_issues");
    let prompt = graph.nodes[2].config["prompt"].as_str().expect("prompt");
    assert!(prompt.contains(".nodes[\"fetch_issues\"].item"), "{prompt}");
}

#[test]
fn a_step_id_starting_with_a_digit_still_compiles_as_jq() {
    // `sanitize_id` keeps `[a-z0-9_]`, which is a wider set than the
    // identifiers jq's dot syntax accepts, and nothing upstream rejects an id
    // beginning with a digit. Spelled `.nodes.2024_report` the whole prompt
    // fails to compile — a plan refused by the evaluator over how its author
    // happened to name a step. Evaluated, not string-matched: asserting the
    // spelling is what let the previous path bug ship.
    let recipe = json!({
        "why": "read a numerically named step",
        "declared": [
            { "name": "repo", "description": "owner/name", "required": true }
        ],
        "inputs": { "repo": "acme/thing" },
        "steps": [
            { "id": "2024 report", "run": "cat report" },
            { "id": "summary", "ask": "Summarise it.", "reads": ["2024 report"] }
        ]
    });
    let (graph, _, _) = lower(&recipe, &[]).expect("lowers");
    assert_eq!(graph.nodes[1].id, "2024_report");
    let prompt = graph.nodes[2].config["prompt"].as_str().expect("prompt");
    let scope = json!({
        "run": { "inputs": { "repo": "acme/thing" } },
        "nodes": { "2024_report": { "item": { "json": { "stdout": "12 findings" } } } }
    });
    let rendered = tinyflows::expr::resolve(&json!(prompt), &scope);
    let rendered = rendered.as_str().unwrap_or_default();
    assert!(
        rendered.contains("12 findings"),
        "a digit-leading step id must produce a compilable path: {prompt} -> {rendered}"
    );
}

// ---------------------------------------------------------------------------
// `use` steps: calling a saved workflow as one step of a plan.
// ---------------------------------------------------------------------------

use super::Callable;

include!("recipe_tests/recipe_part_01_tests.rs");
include!("recipe_tests/recipe_part_02_tests.rs");
