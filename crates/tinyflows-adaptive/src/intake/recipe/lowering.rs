/// Ask steps that restate a declared value instead of relying on the
/// attachment. Only distinctive values count — refusing a plan because an
/// ask contains the word "on" would block perfectly reusable recipes — and
/// only DECLARED inputs: undeclared entries are trimmed by the author gate
/// and never attached, so their values in an ask are just prose.
fn pasted_values(
    steps: &[Step],
    declared: &[(String, String, bool)],
    inputs: &Map<String, Value>,
) -> Vec<String> {
    let mut problems = Vec::new();
    for step in steps {
        let Action::Ask { prompt, .. } = &step.action else {
            continue;
        };
        for (name, _, _) in declared {
            let Some(value) = inputs.get(name).and_then(Value::as_str).map(str::trim) else {
                continue;
            };
            if crate::reuse::distinctive(value) && prompt.contains(value) {
                problems.push(format!(
                    "step `{}` pastes the value of input `{name}` into its ask — remove \
                     it; declared values are attached automatically, and a pasted value \
                     makes the plan single-use",
                    step.id
                ));
            }
        }
    }
    problems
}

/// The generated prompt expression for an ask step.
///
/// A jq program the model never sees: the instruction as a quoted literal,
/// then every declared input, then each read step's output through the path
/// its kind actually produces — `stdout` for a script (with the engine's
/// pre-parsed `stdout_json` unnecessary here: the agent reads text), `text`
/// for an upstream agent. Missing values render as an explicit marker
/// rather than vanishing, because an agent told "output: (missing)" says so
/// instead of improvising.
fn ask_expression(
    prompt: &str,
    reads: &[String],
    steps: &[Step],
    declared: &[(String, String, bool)],
) -> String {
    let mut program = format!("={}", jq_quote(prompt));
    for (name, _, _) in declared {
        program.push_str(&format!(
            " + {} + ((.run.inputs{} // \"(not provided)\") | tostring)",
            jq_quote(&format!("\n\n# Input `{name}`\n")),
            jq_field(name)
        ));
    }
    for read in reads {
        let path = format!(
            "({} // \"(no output)\")",
            output_of(read, kind_of(read, steps))
        );
        program.push_str(&format!(
            " + {} + ({path} | tostring)",
            jq_quote(&format!("\n\n# Output of step `{read}`\n"))
        ));
    }
    program
}

/// Which kind of step `id` is, for choosing how to read its output.
fn kind_of<'a>(id: &str, steps: &'a [Step]) -> Option<&'a Action> {
    steps
        .iter()
        .find(|step| step.id == id)
        .map(|step| &step.action)
}

/// The jq path that yields a step's output as something readable.
///
/// Each node kind puts its result somewhere different, and this is the one
/// place that knows where.
///
/// **An agent's prose is at `item.text`, not `item.json.text`.** The two are
/// siblings on the envelope — `json` is the structured value, `text` is the
/// prose `text_of` derived from it — so `item.json.text` reads a `text` field
/// *inside* the structured value, which a prose reply does not have. It
/// resolved to null, and every `reads` of an agent step rendered
/// "(no output)": the exact silent-null class this whole surface exists to
/// make impossible, sitting inside the surface. A script's `stdout` genuinely
/// is nested (`json` holds `{exit_code, stdout}`), which is what made the two
/// paths look symmetric enough to write side by side.
///
/// For a called workflow it is a projection, because a `sub_workflow` node
/// emits the child's entire final run state, every node of it, wrapped around
/// the answer. Handing an agent that whole object would bury the deliverable
/// in the child's own bookkeeping.
///
/// The projection keeps each child step's readable leaf, labelled with the
/// step id it came from. Not just the last one: the child's node slots are a
/// JSON object, whose key order is alphabetical rather than the order the
/// steps ran, so "the last one" is not a thing this expression can ask for.
/// Everything the child produced, named, is the honest answer — and for the
/// ordinary child whose one agent step writes the deliverable, it is exactly
/// that deliverable.
fn output_of(id: &str, action: Option<&Action>) -> String {
    let slot = jq_field(id);
    match action {
        Some(Action::Run(_)) => format!(".nodes{slot}.item.json.stdout"),
        Some(Action::Use { .. }) => child_answer(id),
        _ => format!(".nodes{slot}.item.text"),
    }
}

/// The projection that turns a called workflow's run state into prose.
///
/// Written defensively at every hop — a slot with no `items`, an empty array,
/// an item whose payload is not an object — because this walks a *child's*
/// state, whose shape this graph did not choose. A failure here would take
/// down the parent node rather than report the step that produced nothing,
/// and a child always has at least one payload that is not an object: its
/// trigger slot holds the seeded item **array**.
///
/// Guarded with an explicit `type == "object"` rather than the `?` operator,
/// which does not do what it looks like it does here. In `jaq`, `.a?` over a
/// non-object yields no output as expected, but a two-hop `.a.b?` fails the
/// whole enclosing expression instead — so the "defensive" spelling of this
/// projection resolved the entire prompt to null, and every composed plan
/// reached its combining agent with nothing. Found by evaluating against a
/// real child run state; a synthetic one whose slots were all objects passed.
fn child_answer(id: &str) -> String {
    // Two different shapes in one expression, which is the whole hazard here.
    // `.nodes.{id}.item` is the *scope* projection — the child's final run
    // state. Inside it, `.nodes.<child>` is a raw run-state slot, which stores
    // serialized items (`{"json": …}`) rather than the bare payloads the scope
    // exposes. So the outer hop drops `items`/`json` and the inner one needs
    // both.
    let slot = jq_field(id);
    format!(
        "([((.nodes{slot}.item.nodes // {{}}) | to_entries[]) \
         | . as $step \
         | ((.value.items // []) | .[-1] | .json) as $out \
         | (($out | if type == \"object\" then \
              (.text // .stdout // (.json | if type == \"object\" then .stdout else null end)) \
            else null end) // empty) as $said \
         | \"## \" + $step.key + \"\\n\" + ($said | tostring)] \
         | join(\"\\n\\n\") \
         | if . == \"\" then null else . end)"
    )
}

/// One object key as a jq path step: `["fetch_pr"]`, never `.fetch_pr`.
///
/// `sanitize_id` keeps `[a-z0-9_]`, which is *not* the same set as the
/// identifiers jq's dot syntax accepts: it permits a leading digit, and nothing
/// upstream rejects a step id such as `2024_report`. `.nodes.2024_report` does
/// not compile, so the whole prompt expression fails at run time — a plan
/// refused by the evaluator for the way its author spelled an id. Bracket
/// access takes any key, so this is the only spelling used for an interpolated
/// name.
fn jq_field(name: &str) -> String {
    format!("[{}]", jq_quote(name))
}

/// A string as a jq literal: quoted, with the characters jq treats specially
/// escaped. Newlines become `\n` so the program stays one line.
fn jq_quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for ch in text.chars() {
        match ch {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

fn parse_steps(
    answer: &Value,
    callables: &[Callable],
    declared: &[(String, String, bool)],
) -> Result<Vec<Step>, IntakeError> {
    let raw = answer["steps"]
        .as_array()
        .filter(|steps| !steps.is_empty())
        .ok_or_else(|| {
            IntakeError::Invalid(
                "the reply has no `steps` — return at least one step with an `id` and a \
                 `run` script, an `ask` instruction or a `use` workflow id"
                    .to_string(),
            )
        })?;

    let mut problems = Vec::new();
    let mut steps: Vec<Step> = Vec::new();
    for (index, step) in raw.iter().enumerate() {
        let id = sanitize_id(step["id"].as_str().unwrap_or_default());
        if id.is_empty() {
            problems.push(format!("step {index} has no usable `id`"));
            continue;
        }
        if id == "start" || steps.iter().any(|existing| existing.id == id) {
            problems.push(format!("step id `{id}` is taken — ids must be unique"));
            continue;
        }
        let run = step["run"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let ask = step["ask"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let called = step["use"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let given = [run.is_some(), ask.is_some(), called.is_some()]
            .iter()
            .filter(|given| **given)
            .count();
        if given > 1 {
            problems.push(format!(
                "step `{id}` has more than one of `run`, `ask` and `use` — a step does \
                 exactly one thing, so split it"
            ));
            continue;
        }
        let action = match (run, ask, called) {
            (Some(script), _, _) => Action::Run(script.to_string()),
            (_, Some(prompt), _) => Action::Ask {
                prompt: prompt.to_string(),
                worker: step["worker"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string),
            },
            (_, _, Some(workflow_id)) => {
                match call(&id, workflow_id, &step["with"], callables, declared, &steps) {
                    Ok(action) => action,
                    Err(problem) => {
                        problems.push(problem);
                        continue;
                    }
                }
            }
            (None, None, None) => {
                problems.push(format!(
                    "step `{id}` has none of a `run` script, an `ask` instruction or a \
                     `use` workflow id"
                ));
                continue;
            }
        };
        let mut reads = Vec::new();
        if let Some(raw_reads) = step["reads"].as_array() {
            for read in raw_reads {
                let read = sanitize_id(read.as_str().unwrap_or_default());
                if steps.iter().any(|existing| existing.id == read) {
                    reads.push(read);
                } else {
                    problems.push(format!(
                        "step `{id}` reads `{read}`, which is not an EARLIER step id"
                    ));
                }
            }
        }
        if matches!(action, Action::Run(_)) && !reads.is_empty() {
            problems.push(format!(
                "step `{id}`: `reads` only works on ask steps — a run script sees nothing"
            ));
        }
        if matches!(action, Action::Use { .. }) && !reads.is_empty() {
            problems.push(format!(
                "step `{id}`: `reads` only works on ask steps — a called workflow takes \
                 what you pass it, so put `\"@step.<id>\"` in `with` instead"
            ));
        }
        steps.push(Step { id, action, reads });
    }
    if !problems.is_empty() {
        return Err(IntakeError::Invalid(problems.join("; ")));
    }
    Ok(steps)
}

/// Build one `use` step, refusing everything that could only fail later.
///
/// Three checks, and each stands for a run this saves: an id nobody offered
/// is a hallucination that the resolver would turn into a mid-run capability
/// error; a required input left unfilled fails the child's own declaration
/// check after the parent has already spent its earlier steps; and a `with`
/// key the child never declared is a value the model believes it is passing
/// and the child will never see.
fn call(
    id: &str,
    workflow_id: &str,
    with: &Value,
    callables: &[Callable],
    declared: &[(String, String, bool)],
    earlier: &[Step],
) -> Result<Action, String> {
    let Some(callable) = callables.iter().find(|c| c.id == workflow_id) else {
        let offered: Vec<&str> = callables.iter().map(|c| c.id.as_str()).collect();
        return Err(if offered.is_empty() {
            format!(
                "step `{id}` uses `{workflow_id}`, but this host has no saved workflows to \
                 call — write the step yourself"
            )
        } else {
            format!(
                "step `{id}` uses `{workflow_id}`, which is not one of the workflows you \
                 can call ({})",
                offered.join(", ")
            )
        });
    };

    let given = match with {
        Value::Null => Map::new(),
        Value::Object(fields) => fields.clone(),
        _ => {
            return Err(format!(
                "step `{id}`: `with` must be an object mapping `{workflow_id}`'s input \
                 names to values"
            ));
        }
    };

    for (name, _) in callable.inputs.iter().filter(|(_, required)| *required) {
        // Present-but-empty is not supplied. `"repo": null` and `"repo": ""`
        // pass a `contains_key` check and are then forwarded unchanged, so the
        // child fails its own declaration check mid-run — the exact failure
        // this refusal exists to move to intake. `gated` in `author.rs` already
        // reads unfilled the same way; the two must not disagree.
        let filled = given
            .get(name)
            .is_some_and(|value| !value.is_null() && value.as_str() != Some(""));
        if !filled {
            return Err(format!(
                "step `{id}`: `{workflow_id}` requires the input `{name}` and `with` does \
                 not supply it"
            ));
        }
    }
    let mut forwarded = Map::new();
    for (name, value) in given {
        if !callable.inputs.iter().any(|(input, _)| input == &name) {
            return Err(format!(
                "step `{id}`: `{workflow_id}` declares no input `{name}` — it takes {}",
                if callable.inputs.is_empty() {
                    "none".to_string()
                } else {
                    callable
                        .inputs
                        .iter()
                        .map(|(input, _)| input.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ));
        }
        forwarded.insert(
            name,
            forward(&value, declared, earlier).map_err(|why| format!("step `{id}`: {why}"))?,
        );
    }
    Ok(Action::Use {
        workflow_id: workflow_id.to_string(),
        with: forwarded,
    })
}

/// Turn one `with` value into what the child should receive.
///
/// `@input.x` and `@step.y` become the engine expressions that read them; a
/// plain value is passed as itself. The sigil exists so a model can wire a
/// child's input to live data without writing jq — the same bargain the rest
/// of this surface makes.
fn forward(
    value: &Value,
    declared: &[(String, String, bool)],
    earlier: &[Step],
) -> Result<Value, String> {
    let Some(reference) = value.as_str().and_then(|text| text.strip_prefix('@')) else {
        return Ok(value.clone());
    };
    if let Some(name) = reference.strip_prefix("input.") {
        let name = sanitize_id(name);
        if !declared.iter().any(|(declared, _, _)| declared == &name) {
            return Err(format!(
                "`@input.{name}` names an input you did not declare — add it to `declared`"
            ));
        }
        return Ok(Value::String(format!("=.run.inputs{}", jq_field(&name))));
    }
    if let Some(step) = reference.strip_prefix("step.") {
        let step = sanitize_id(step);
        let Some(action) = kind_of(&step, earlier) else {
            return Err(format!(
                "`@step.{step}` names a step that is not an EARLIER step of this plan"
            ));
        };
        return Ok(Value::String(format!(
            "={}",
            output_of(&step, Some(action))
        )));
    }
    Err(format!(
        "`{reference}` is not a reference this understands — write `@input.<name>`, \
         `@step.<id>`, or a plain value"
    ))
}

fn parse_declared(answer: &Value) -> Vec<(String, String, bool)> {
    answer["declared"]
        .as_array()
        .map(|declared| {
            declared
                .iter()
                .filter_map(|input| {
                    let name = sanitize_id(input["name"].as_str().unwrap_or_default());
                    if name.is_empty() {
                        return None;
                    }
                    Some((
                        name,
                        input["description"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        input["required"].as_bool().unwrap_or(false),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A graph name from the recipe: the first ask step's opening words, or the
/// step ids — something a shelf listing can show, not an id.
fn graph_name(why: &str, steps: &[Step]) -> String {
    let head: String = why.split_whitespace().take(6).collect::<Vec<_>>().join(" ");
    if !head.is_empty() {
        return head;
    }
    steps
        .iter()
        .map(|step| step.id.as_str())
        .collect::<Vec<_>>()
        .join(" → ")
}

/// Identifiers the engine and jq both accept: lowercase, alnum and `_`.
fn sanitize_id(raw: &str) -> String {
    let mut id: String = raw
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    while id.starts_with('_') {
        id.remove(0);
    }
    while id.ends_with('_') {
        id.pop();
    }
    id
}

