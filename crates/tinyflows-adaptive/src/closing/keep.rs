//! Turning a graph that worked into a procedure that stays.
//!
//! The missing half of *"selects a stored workflow or authors one"*. Authoring
//! ran, produced a graph, the graph achieved the goal — and then the graph was
//! discarded, so the next episode of the same shape authored it again from
//! nothing. The catalogue only ever held what a person had put there, and
//! `select` could never choose something the loop itself worked out.
//!
//! Three gates, cheapest first, and each rules out a different kind of mistake.
//!
//! 1. **It has to have worked.** A graph that fell short is the repair path's
//!    business, not this one's.
//! 2. **It has to be reusable** — [`crate::reuse::baked_in`], which is exact
//!    rather than a judgement: an input value pasted into a node instead of
//!    read through a binding means the graph matches one task and never
//!    another. No model is asked, because a fuzzy gate on a store that grows
//!    forever is a store that fills with near-misses.
//! 3. **It has to be describable as a class.** Only then is a model asked, and
//!    only for prose — the graph is already fixed. `select` reads descriptions
//!    to choose, so a stored workflow described by the goal that produced it
//!    ("summarise /docs/q3.pdf") is unfindable by the next goal of its kind.
//!
//! The name and description are the whole of what inference contributes here,
//! and the [`Tier::Generalise`] request says so. It is the same judgement the
//! consolidator makes about a lesson's `trigger`: describe the situation, never
//! the instance.

use std::sync::Arc;

use tinyflows::caps::Capabilities;
use tinyflows::model::WorkflowGraph;
use tinyflows::store::{WorkflowRecord, WorkflowStore};

use crate::contracts::{Goal, Tier};
use crate::intake::{IntakeError, Result, ask};
use crate::reuse::baked_in;

const SYSTEM: &str = "\
You name a workflow that just achieved a goal, so it can be found again.

Return JSON: {\"name\": str, \"description\": str, \"reusable\": bool}

The graph is finished and you are not editing it. You are writing the two lines
a planner reads when deciding whether this procedure does what a NEW goal asks.

- name: a few words. What it does, not what it was for.
- description: one or two sentences naming the CLASS of task and what the
  workflow needs to be given. It is the only thing a planner sees besides the
  step count, so a description that restates the original goal makes this
  findable exactly once.

    good  \"Reviews the open pull requests on a repository and posts a summary.
           Takes the repository as an input.\"
    bad   \"Reviews the open PRs on acme/thing.\"          — names one instance
    bad   \"Does the thing that was asked.\"               — names nothing

- reusable: false when this graph only makes sense for the one goal it was
  written for, whatever its inputs say. A one-off kept in the catalogue is a row
  every future planner reads and none can use, so say so rather than reaching
  for a description that sounds general.

  Also false when the goal's specifics — a topic, a name, a repository, a
  value — sit inside a node's prompt or config instead of arriving through a
  declared input. Run unchanged for the NEXT goal of its class, that graph
  does the old goal's specific thing, which is worse than not being found:
  it is found, run, and wrong. Reusable means reusable as-is.";

/// What was kept, when anything was.
#[derive(Debug, Clone)]
pub struct Kept {
    /// The stored record. Its id is derived from the graph's shape, so the same
    /// procedure arrived at twice converges rather than accumulating.
    pub record: WorkflowRecord,
    /// The class of task it was described as, in the model's words.
    pub description: String,
}

/// Keep an authored graph that achieved its goal, if it is worth keeping.
///
/// `Ok(None)` is the ordinary answer and not a failure: the graph baked its
/// specifics in, or the model judged it a one-off.
///
/// # Errors
/// When inference fails, or the store refuses the record.
pub async fn keep(
    goal: &Goal,
    graph: &WorkflowGraph,
    inputs: &serde_json::Map<String, serde_json::Value>,
    store: &Arc<dyn WorkflowStore>,
    caps: &Capabilities,
    conn: Option<&str>,
) -> Result<Option<Kept>> {
    // Exact, and free. A graph that pasted its inputs matches one task, and no
    // description can make it match another.
    let pasted = baked_in(graph, inputs);
    if !pasted.is_empty() {
        return Ok(None);
    }

    let declared = if graph.inputs.is_empty() {
        "(none)".to_string()
    } else {
        graph
            .inputs
            .iter()
            .map(|input| format!("- {}", input.name))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let user = format!(
        "# The goal it achieved\n{}\n\n# Its declared inputs\n{declared}\n\n# The graph\n{}",
        goal.text.trim(),
        serde_json::to_string_pretty(graph).map_err(|e| IntakeError::Store(e.to_string()))?
    );

    let answer = ask(caps, conn, Tier::Generalise, SYSTEM, &user).await?;
    if !answer["reusable"].as_bool().unwrap_or(false) {
        return Ok(None);
    }
    let description = answer["description"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    // A workflow nobody can choose on purpose is a row that costs a planner
    // attention and returns nothing, so an empty description is a refusal.
    if description.is_empty() {
        return Ok(None);
    }

    let name = answer["name"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    let id = crate::reuse::shape_id(graph);
    let record = WorkflowRecord {
        id: id.clone(),
        name: if name.is_empty() { id.clone() } else { name },
        description,
        enabled: true,
        defaults: tinyflows::store::types::WorkflowDefaults::default(),
        graph: WorkflowGraph {
            id: Some(id),
            ..graph.clone()
        },
        // Never inherited and never invented: this graph came from a model, not
        // from a file, and claiming a path would make the store think it owns
        // something on disk.
        source_path: None,
    };
    store
        .save(&record)
        .map_err(|e| IntakeError::Store(e.to_string()))?;

    let description = record.description.clone();
    Ok(Some(Kept {
        record,
        description,
    }))
}
