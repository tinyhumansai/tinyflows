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

include!("recipe/lowering.rs");
#[cfg(test)]
#[path = "recipe_tests.rs"]
mod tests;
