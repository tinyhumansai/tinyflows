fn audit() -> Callable {
    Callable {
        id: "pr-audit-review".to_string(),
        name: "PR audit review".to_string(),
        description: "reviews a pull request and posts the verdict".to_string(),
        inputs: vec![("repo".to_string(), true), ("depth".to_string(), false)],
    }
}

fn compose_recipe() -> serde_json::Value {
    json!({
        "why": "audit the PR, then summarise what it found",
        "declared": [
            { "name": "repo", "description": "owner/name", "required": true }
        ],
        "inputs": { "repo": "acme/thing" },
        "steps": [
            { "id": "audit", "use": "pr-audit-review", "with": { "repo": "@input.repo" } },
            { "id": "summary", "ask": "Summarise the audit in three bullets.",
              "reads": ["audit"] }
        ]
    })
}

#[test]
fn a_use_step_lowers_to_a_sub_workflow_node_that_references_the_callee() {
    let (graph, _, _) = lower(&compose_recipe(), &[audit()]).expect("lowers");
    assert!(
        validate_all(&graph).is_empty(),
        "{:?}",
        validate_all(&graph)
    );
    let node = graph
        .nodes
        .iter()
        .find(|node| node.id == "audit")
        .expect("the use step became a node");
    assert_eq!(node.kind, NodeKind::SubWorkflow);
    // By reference, never inlined: the callee keeps its own identity, its own
    // scores, and whatever it becomes next.
    assert_eq!(node.config["workflow_id"], json!("pr-audit-review"));
    assert!(
        node.config.get("workflow").is_none(),
        "an inlined child would fork the callee at authoring time"
    );
    // `@input.repo` became the expression that reads the parent's run input.
    assert_eq!(
        node.config["inputs"]["repo"],
        json!("=.run.inputs[\"repo\"]")
    );
}

#[test]
fn the_lowered_sub_workflow_config_satisfies_the_engines_own_contract() {
    // The same drift guard the shell lowering has, for the same reason: the
    // `use` step's whole value is that the engine already knows how to run a
    // child, and it knows it by reading `workflow_id` and `inputs`. A rename on
    // either side would surface as a capability error mid-run, attributed to
    // the work rather than to the plan.
    let (graph, _, _) = lower(&compose_recipe(), &[audit()]).expect("lowers");
    let config = &graph
        .nodes
        .iter()
        .find(|node| node.id == "audit")
        .expect("the use step became a node")
        .config;
    let contract = tinyflows::catalog::all_contracts()
        .iter()
        .find(|contract| contract.kind == "sub_workflow")
        .expect("the engine has a sub_workflow contract")
        .clone();
    let fields: Vec<&str> = contract
        .config_fields
        .iter()
        .map(|field| field.name.as_str())
        .collect();
    for key in ["workflow_id", "inputs"] {
        assert!(
            fields.contains(&key),
            "the engine's sub_workflow contract no longer declares `{key}`: {fields:?}"
        );
        assert!(
            config.get(key).is_some(),
            "a lowered use step must fill config.{key}: {config}"
        );
    }
    // Required fields are the engine's own list; filling one is not enough if
    // it grows another.
    for field in contract.config_fields.iter().filter(|field| field.required) {
        assert!(
            config.get(&field.name).is_some(),
            "the lowering fills none of the engine's required sub_workflow field \
             `{}`: {config}",
            field.name
        );
    }
}

#[test]
fn a_step_reference_in_with_reads_the_earlier_step_the_way_its_kind_produces() {
    let recipe = json!({
        "why": "fetch the diff, then hand it to a saved reviewer",
        "declared": [],
        "steps": [
            { "id": "diff", "run": "git diff" },
            { "id": "review", "use": "reviewer", "with": { "patch": "@step.diff" } }
        ]
    });
    let callable = Callable {
        id: "reviewer".to_string(),
        name: String::new(),
        description: "reviews a patch".to_string(),
        inputs: vec![("patch".to_string(), true)],
    };
    let (graph, _, _) = lower(&recipe, &[callable]).expect("lowers");
    let node = graph
        .nodes
        .iter()
        .find(|node| node.id == "review")
        .expect("the use step became a node");
    // A run step's output is its stdout, and the reference knows that without
    // the model having to.
    assert_eq!(
        node.config["inputs"]["patch"],
        json!("=.nodes[\"diff\"].item.json.stdout")
    );
}

#[test]
fn a_required_input_present_but_empty_is_refused_the_way_an_absent_one_is() {
    // `contains_key` accepts `null` and `""`, which `forward` then passes
    // through unchanged — so the child fails its OWN declaration check
    // mid-run, which is the failure this refusal exists to move to intake.
    // `gated` in `author.rs` already reads unfilled this way; the two checks
    // disagreeing is what let the value through.
    for empty in [json!(null), json!("")] {
        let recipe = json!({
            "why": "audit the PR",
            "declared": [],
            "steps": [
                { "id": "audit", "use": "pr-audit-review", "with": { "repo": empty } }
            ]
        });
        let err = lower(&recipe, &[audit()]).expect_err("refused").to_string();
        assert!(
            err.contains("requires the input `repo`"),
            "an empty value is not a supplied value ({empty}): {err}"
        );
    }
}

#[test]
fn a_use_step_naming_a_workflow_nobody_offered_is_refused_at_intake() {
    // Not deferred to the resolver: a hallucinated id would surface as a
    // capability error mid-run, after the earlier steps had already been paid
    // for, and be attributed to the work rather than to the plan.
    let err = lower(&compose_recipe(), &[])
        .expect_err("refused")
        .to_string();
    assert!(err.contains("no saved workflows to call"), "{err}");

    let other = Callable {
        id: "something-else".to_string(),
        ..audit()
    };
    let err = lower(&compose_recipe(), &[other])
        .expect_err("refused")
        .to_string();
    assert!(
        err.contains("something-else"),
        "names what IS callable: {err}"
    );
}

#[test]
fn a_use_step_that_omits_a_required_input_is_refused_with_the_name() {
    let recipe = json!({
        "why": "call it with nothing",
        "steps": [{ "id": "audit", "use": "pr-audit-review", "with": {} }]
    });
    let err = lower(&recipe, &[audit()]).expect_err("refused").to_string();
    assert!(err.contains("requires the input `repo`"), "{err}");
}

#[test]
fn a_with_key_the_callee_never_declared_is_refused_rather_than_dropped() {
    // Silently dropping it would leave the model believing it passed a value
    // the child will never see — the worst kind of pass, because the run
    // completes.
    let recipe = json!({
        "why": "wrong input name",
        "declared": [{ "name": "repo", "description": "owner/name", "required": true }],
        "steps": [{
            "id": "audit", "use": "pr-audit-review",
            "with": { "repo": "@input.repo", "reponame": "acme/thing" }
        }]
    });
    let err = lower(&recipe, &[audit()]).expect_err("refused").to_string();
    assert!(err.contains("declares no input `reponame`"), "{err}");
    assert!(err.contains("repo, depth"), "says what it does take: {err}");
}

#[test]
fn an_input_reference_to_something_undeclared_is_refused() {
    let recipe = json!({
        "why": "reference an input that does not exist",
        "declared": [],
        "steps": [{
            "id": "audit", "use": "pr-audit-review", "with": { "repo": "@input.repo" }
        }]
    });
    let err = lower(&recipe, &[audit()]).expect_err("refused").to_string();
    assert!(err.contains("did not declare"), "{err}");
}

#[test]
fn reads_on_a_use_step_points_at_with_instead() {
    let recipe = json!({
        "why": "reads does not apply",
        "declared": [{ "name": "repo", "description": "owner/name", "required": true }],
        "steps": [
            { "id": "diff", "run": "git diff" },
            { "id": "audit", "use": "pr-audit-review", "reads": ["diff"],
              "with": { "repo": "@input.repo" } }
        ]
    });
    let err = lower(&recipe, &[audit()]).expect_err("refused").to_string();
    assert!(err.contains("`with`"), "{err}");
}

#[test]
fn a_step_reading_a_use_step_gets_the_childs_answer_not_its_run_state() {
    // The defect this projection exists for: a `sub_workflow` node emits the
    // child's ENTIRE final run state, so a naive read hands the next agent the
    // child's bookkeeping with the deliverable buried in it.
    let (graph, _, _) = lower(&compose_recipe(), &[audit()]).expect("lowers");
    let summary = graph
        .nodes
        .iter()
        .find(|node| node.id == "summary")
        .expect("the ask step");
    let prompt = summary.config["prompt"]
        .as_str()
        .expect("a generated prompt expression");

    // Run the generated expression against a real child run state, through the
    // engine's own evaluator — the only thing that proves the projection is
    // valid jq and picks the right leaves.
    let state = json!({
        "run": { "inputs": { "repo": "acme/thing" } },
        "inputs": { "repo": "acme/thing" },
        "nodes": { "audit": { "item": child_run_state(), "items": [child_run_state()] } }
    });
    let rendered = tinyflows::expr::resolve(&json!(prompt), &state);
    let rendered = rendered.as_str().expect("resolves to a string");

    assert!(
        rendered.contains("Requesting changes."),
        "the child's deliverable must reach the reader: {rendered}"
    );
    assert!(
        rendered.contains("3 files changed"),
        "and so must every other leaf it produced: {rendered}"
    );
    assert!(
        rendered.contains("## verdict"),
        "each labelled with the child step it came from: {rendered}"
    );
    assert!(
        !rendered.contains("trigger"),
        "but not the child's own bookkeeping: {rendered}"
    );
}

#[test]
fn a_child_that_produced_nothing_readable_says_so_rather_than_erroring() {
    // The projection walks a state this graph did not choose the shape of, so
    // every hop is written defensively; a jq error here would fail the parent
    // node instead of reporting the step that produced nothing.
    let (graph, _, _) = lower(&compose_recipe(), &[audit()]).expect("lowers");
    let prompt = graph
        .nodes
        .iter()
        .find(|node| node.id == "summary")
        .expect("the ask step")
        .config["prompt"]
        .as_str()
        .expect("a generated prompt expression")
        .to_string();

    for state in [
        json!({ "run": {}, "nodes": { "audit": { "item": { "nodes": {} }, "items": [] } } }),
        json!({ "run": {}, "nodes": { "audit": { "item": null, "items": [] } } }),
        json!({ "run": {}, "nodes": {} }),
    ] {
        let rendered = tinyflows::expr::resolve(&json!(prompt), &state);
        let rendered = rendered.as_str().unwrap_or_default();
        assert!(
            rendered.contains("(no output)"),
            "empty child state {state} rendered: {rendered}"
        );
    }
}

#[test]
fn the_callable_listing_names_the_inputs_a_call_must_fill() {
    // A model asked to supply `with` for inputs it was never shown is a model
    // guessing — the same defect the chooser had.
    let rendered = super::render_callables(&[audit()]);
    assert!(rendered.contains("pr-audit-review"), "{rendered}");
    assert!(
        rendered.contains("with: repo, depth (optional)"),
        "{rendered}"
    );
    assert!(
        super::render_callables(&[]).is_empty(),
        "a cold store offers no `use` list at all, rather than an empty one"
    );
}
