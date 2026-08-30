//! The simple authoring surface: a recipe of steps, lowered to a real graph.
//!
//! Six field-test runs established why full-graph authoring fails at small
//! model tiers: the author must emit exact tokens across three foreign
//! syntaxes at once — the graph dialect (`=`-expressions, envelope paths),
//! the host's tools, and whatever CLI its scripts drive — blind, with
//! feedback one round away. One wrong token anywhere is a dead graph, and
//! the defects surface serially.
//!
//! So the model does not write graphs. It writes a **recipe** — steps that
//! either `run` a script or `ask` an agent, with `reads` naming which
//! earlier steps' output an agent needs — and [`lower`] compiles that into a
//! valid [`WorkflowGraph`] deterministically. Every expression, envelope
//! path and edge is generated here, by code that knows the engine's shapes
//! exactly. The model's remaining obligations are things models are good
//! at: choosing steps, writing commands, writing prose.
//!
//! The lowered graph still walks every downstream gate. By construction it
//! should pass them all; a construction bug surfacing as a refusal instead
//! of a run-time null is the point of keeping them.

use serde_json::{Map, Value, json};
use tinyflows::model::{Edge, InputType, Node, NodeKind, WorkflowGraph, WorkflowInput};

use super::IntakeError;

/// The authoring prompt for the recipe surface.
///
/// Deliberately free of graph syntax: nothing here teaches nodes, edges,
/// bindings or envelopes, because the model never writes them.
pub const SYSTEM: &str = "\
You plan how to achieve a goal as a short sequence of steps.

Return JSON:
{\"why\": str,
 \"declared\": [{\"name\": str, \"description\": str, \"required\": bool}],
 \"inputs\": {name: value},
 \"steps\": [
   {\"id\": str, \"run\": str},
   {\"id\": str, \"ask\": str, \"reads\": [str], \"worker\": str?},
   {\"id\": str, \"use\": str, \"with\": {name: value}}
 ]}

- Steps execute in the order listed. Each step has an `id` (a short
  snake_case name) and exactly ONE of:
    run  a shell script. It must PRINT its result to stdout — a result in a
         file or nowhere is a result the next step cannot see. Print JSON
         when the output is structured.
    ask  an instruction for an AI agent. Say exactly what to produce and
         that it should be produced directly. The agent's reply text is the
         step's output.
    use  the id of a saved workflow, from the list of ones you can call. It
         runs as one step and its output is what it produced.
- `reads` (ask steps only): the ids of EARLIER steps whose output this agent
  needs. Their output is attached to the instruction automatically — do not
  describe how to fetch it, and do not use placeholders for it.
- `worker` (ask steps only, optional): a worker this host lists, when the
  step must run somewhere specific. Omit it otherwise.
- `with` (use steps only): a value for each input the saved workflow declares.
  Write `\"@input.<name>\"` to pass one of YOUR declared inputs through, or
  `\"@step.<id>\"` to pass an earlier step's output. Anything else is used as
  a literal value.
- `declared`: the workflow's inputs — anything the goal supplies as data (a
  repository, a topic, an id), so the plan works for the NEXT goal of its
  kind with different values. `inputs` supplies this run's value for every
  required one. Declared values are attached to ask steps automatically —
  NEVER also paste a value into an ask: a pasted value makes the plan
  single-use, so it cannot be kept for future goals, and it is refused.
- The LAST step's output is the run's answer: make it the step that produces
  the deliverable.

Keep it short. One step is often right: an agent asked for the whole
deliverable, with the goal's data declared as inputs. Use `run` steps for
deterministic fetching or checking, not for judgement.

A `use` step is for a WHOLE job somebody already solved — the goal asks for
two of them, or for one plus your own work on top. It is not for scavenging:
if you only want part of what a saved workflow does, write the step yourself.

Where a section below states what this host permits, it is enforced when the
plan runs. Where a section lists what this episode already tried, produce a
DIFFERENT plan, not the same one reworded.";

/// One saved workflow a plan may call as a step.
///
/// The same rows the chooser weighs, minus its scores: composition asks "does
/// this job exist" rather than "is this the whole answer", so the record that
/// decides a *selection* is noise here — and unlike the chooser's list this
/// one is not filtered by what the episode already tried, because a workflow
/// that fell short alone is exactly the one worth calling as a part.
#[derive(Debug, Clone)]
pub struct Callable {
    /// The id a `use` step names.
    pub id: String,
    /// Display name; falls back to the id when blank.
    pub name: String,
    /// What it does — the only thing that can justify calling it.
    pub description: String,
    /// Its declared inputs: name and whether it is required.
    pub inputs: Vec<(String, bool)>,
}

impl Callable {
    /// The prompt listing for one callable workflow.
    fn render(&self) -> String {
        let name = if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        };
        let description = if self.description.is_empty() {
            "(no description — do not call this; nobody can say what it does)"
        } else {
            &self.description
        };
        let inputs = if self.inputs.is_empty() {
            "takes no inputs".to_string()
        } else {
            format!("with: {}", render_inputs(&self.inputs))
        };
        format!(
            "- id: {}\n  name: {name}\n  {inputs}\n  {description}",
            self.id
        )
    }
}

/// Declared inputs as one comma-separated listing: `repo, depth (optional)`.
///
/// A required input is named bare and an optional one is marked, because the
/// only thing a planner does with this line is decide what it must supply. Both
/// prompts that carry it — the chooser's candidate listing and the author's
/// callable listing — render it here rather than each spelling the rule out,
/// since two copies of a convention a model is being asked to obey drift the
/// first time the wording changes and then teach two different things.
///
/// The prefix and the empty-list wording stay with each caller: the chooser
/// says nothing at all when there are no inputs, the author says "takes no
/// inputs", and that difference is deliberate.
pub(super) fn render_inputs(inputs: &[(String, bool)]) -> String {
    inputs
        .iter()
        .map(|(name, required)| {
            if *required {
                name.clone()
            } else {
                format!("{name} (optional)")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The prompt section listing what a `use` step may name.
///
/// Empty when nothing is callable, so a cold store's author is not told about
/// a step kind it cannot use — an offer with an empty list reads as a missing
/// list, and the model invents an id to fill it.
#[must_use]
pub fn render_callables(callables: &[Callable]) -> String {
    if callables.is_empty() {
        return String::new();
    }
    let listed: Vec<String> = callables.iter().map(Callable::render).collect();
    format!(
        "# Saved workflows you can call with a `use` step\n{}",
        listed.join("\n")
    )
}

/// One parsed step of a recipe.
struct Step {
    id: String,
    action: Action,
    reads: Vec<String>,
}

enum Action {
    Run(String),
    Ask {
        prompt: String,
        worker: Option<String>,
    },
    /// Call a saved workflow, forwarding values for its declared inputs.
    Use {
        workflow_id: String,
        with: Map<String, Value>,
    },
}

/// Lower a recipe reply into a runnable graph plus its run values.
///
/// # Errors
/// [`IntakeError::Invalid`] naming every structural problem at once — absent
/// steps, duplicate or malformed ids, `reads` pointing forward or nowhere, a
/// step that is both `run` and `ask` or neither. The messages are written
/// for the feedback round: each states the fix, not just the fault.
pub fn lower(
    answer: &Value,
    callables: &[Callable],
) -> Result<(WorkflowGraph, Map<String, Value>, String), IntakeError> {
    let why = answer["why"].as_str().unwrap_or_default().to_string();
    let declared = parse_declared(answer);
    let steps = parse_steps(answer, callables, &declared)?;
    let inputs = answer["inputs"].as_object().cloned().unwrap_or_default();

    // A declared value pasted into an ask defeats the declaration: the
    // lowering attaches the value anyway, so the paste is redundant now and
    // poisonous later — selected for a different value, the prompt would
    // carry BOTH, and the keep gate would rightly refuse to file the plan.
    // Refused here, where the feedback round can fix it, rather than
    // discovered as an unkeepable graph after a satisfied run.
    let pasted = pasted_values(&steps, &declared, &inputs);
    if !pasted.is_empty() {
        return Err(IntakeError::Invalid(pasted.join("; ")));
    }

    let mut nodes = vec![Node {
        id: "start".into(),
        kind: NodeKind::Trigger,
        type_version: 1,
        name: "start".into(),
        config: json!({ "trigger_kind": "manual" }),
        ports: Vec::new(),
        position: None,
    }];
    let mut edges = Vec::new();
    let mut previous = "start".to_string();

    for step in &steps {
        let node = match &step.action {
            Action::Run(script) => Node {
                id: step.id.clone(),
                kind: NodeKind::Shell,
                type_version: 1,
                name: step.id.clone(),
                // `source`, not `script`: the engine's shell node reads
                // config.source — the drift test below ties this key to the
                // engine's own contract so it cannot silently rot again.
                config: json!({ "source": script }),
                ports: Vec::new(),
                position: None,
            },
            Action::Ask { prompt, worker } => {
                let mut config = json!({
                    "prompt": ask_expression(prompt, &step.reads, &steps, &declared)
                });
                if let Some(worker) = worker {
                    config["agent_ref"] = json!(worker);
                }
                Node {
                    id: step.id.clone(),
                    kind: NodeKind::Agent,
                    type_version: 1,
                    name: step.id.clone(),
                    config,
                    ports: Vec::new(),
                    position: None,
                }
            }
            Action::Use { workflow_id, with } => Node {
                id: step.id.clone(),
                kind: NodeKind::SubWorkflow,
                type_version: 1,
                name: step.id.clone(),
                // `workflow_id`, not an inline graph: the child is resolved
                // from the store at run time, so the callee keeps its own
                // identity, its own scores, and whatever it becomes next.
                config: json!({ "workflow_id": workflow_id, "inputs": with }),
                ports: Vec::new(),
                position: None,
            },
        };
        nodes.push(node);
        edges.push(Edge {
            from_node: previous.clone(),
            from_port: "main".into(),
            to_node: step.id.clone(),
            to_port: "main".into(),
        });
        previous = step.id.clone();
    }

    let graph = WorkflowGraph {
        schema_version: 1,
        id: None,
        name: graph_name(&why, &steps),
        inputs: declared
            .iter()
            .map(|(name, description, required)| {
                let input = WorkflowInput::new(name.clone(), InputType::String)
                    .with_description(description.clone());
                if *required { input.required() } else { input }
            })
            .collect(),
        agents: Vec::new(),
        nodes,
        edges,
    };
    Ok((graph, inputs, why))
}

/// The one-step graph an [`Errand`](crate::contracts::Approach::Errand) runs.
///
/// Built by handing [`lower`] a recipe nobody wrote, rather than assembling
/// nodes directly. The graph is trivial enough that hand-building it would look
/// like the simpler option, and that is the trap: it would be a second,
/// unexercised definition of what an `ask` step compiles to, and the first
/// change to the envelope path or the trigger node would leave errands quietly
/// producing a shape the rest of the loop no longer reads. Going through the
/// same door as an authored plan costs one `json!` and cannot drift.
///
/// Deterministic in the goal alone — no model call. The whole economic argument
/// for the errand path is that recognising one is free once `select` has
/// answered, so lowering it must not spend anything either.
///
/// # Errors
/// Only if `lower` refuses this recipe, which would mean the shared lowering
/// path had stopped accepting a bare `ask` step.
pub(crate) fn errand(goal: &str) -> Result<WorkflowGraph, IntakeError> {
    let recipe = json!({
        "why": "one turn of work, no procedure in it",
        "declared": [],
        "inputs": {},
        "steps": [{ "id": "errand", "ask": goal.trim() }],
    });
    lower(&recipe, &[]).map(|(graph, _, _)| graph)
}

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

#[cfg(test)]
#[path = "recipe_tests.rs"]
mod tests;
