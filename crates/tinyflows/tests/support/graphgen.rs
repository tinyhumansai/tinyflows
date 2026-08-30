//! A generator of valid, non-trivial workflow graphs, for property tests.
//!
//! # Why a shape grammar rather than random edges
//!
//! Wiring random nodes to random nodes almost never produces a graph the
//! validator accepts — the overwhelming majority of such graphs are rejected
//! for a missing trigger, an unreachable node, or an unbounded cycle. A
//! generator built that way spends its whole budget proving that
//! [`tinyflows::validate`] says no, and never reaches the engine, which is the
//! part under test.
//!
//! So graphs are built **compositionally** from [`Shape`]s that are correct by
//! construction. Every shape has exactly one entry node and one exit node, so
//! shapes nest and chain without any shape needing to know its context. What
//! gets randomised is the *structure* — nesting, arity, branch depth — not the
//! wiring.
//!
//! # The oracle problem
//!
//! A property test needs to know what the right answer is. Rather than predict
//! outputs, the leaves here are **pure control-flow nodes** (`output_parser`
//! passthroughs and `transform`s over constant values), so no capability is
//! consulted and the whole run is a deterministic function of the graph. That
//! turns "is the output correct?" — which needs a second implementation to
//! answer — into the properties actually worth asserting: it terminates, it is
//! deterministic, it survives a resume. Those hold regardless of what the
//! output *is*.

use proptest::prelude::*;
use serde_json::{Value, json};

use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

/// A structural template for part of a graph.
///
/// Every variant compiles to a subgraph with a single entry and a single exit,
/// which is the invariant that lets them nest and chain freely.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    /// `n` passthrough nodes in a row. `n == 0` is a single node, so a linear
    /// shape always has something to be an entry and an exit.
    Linear(usize),
    /// A `condition` whose `true`/`false` ports run different shapes and rejoin
    /// at a `merge`. Exercises conditional routing and the barrier relief that
    /// keeps an untaken branch from deadlocking the join.
    Branch(Box<Shape>, Box<Shape>),
    /// Two or more shapes fanned out from one port and rejoined at a `merge`.
    /// This is the parallel case: every branch runs in the same super-step.
    Fanout(Vec<Shape>),
    /// A bounded `loop` whose `body` runs a shape and returns to the head.
    Loop {
        /// The loop's `max_iterations` cap.
        max_iter: u64,
        /// What runs on each pass.
        body: Box<Shape>,
    },
    /// `n` tasks spawned without blocking, collected by a gate on `release`.
    ///
    /// The async pair. Present so generated graphs exercise the poll loop and
    /// the release policies against structures nobody hand-picked — a gate
    /// downstream of a branch, inside a loop body, beside a fan-out.
    Spawned {
        /// How many tasks to spawn.
        tasks: usize,
        /// The gate's release policy, as its config `release` value.
        release: &'static str,
        /// `n` for `first_n`/`quorum`; ignored otherwise.
        n: usize,
    },
    /// A sub-workflow running a nested shape.
    ///
    /// Depth-bounded by the generator itself rather than by the engine's cap,
    /// so a generated graph never fails merely for nesting too deep.
    Nested(Box<Shape>),
    /// A node that pauses the run awaiting approval, followed by a shape.
    ///
    /// Present so generated runs actually *suspend*, which is the only way to
    /// exercise the checkpoint/resume path. A shape containing one of these
    /// cannot complete without either a pre-approval on the run input or a
    /// resume that names its gate.
    Gate(Box<Shape>),
}

impl Shape {
    /// An upper bound on the super-steps a run of this shape can take.
    ///
    /// Used to give a generated graph a `recursion_limit` that is generous
    /// enough never to be the reason a run fails, so a run that *does* hit a
    /// bound has found something real. Deliberately an over-estimate.
    fn step_budget(&self) -> u64 {
        match self {
            Self::Linear(n) => *n as u64 + 1,
            Self::Branch(a, b) => 2 + a.step_budget().max(b.step_budget()),
            Self::Fanout(branches) => 2 + branches.iter().map(Self::step_budget).max().unwrap_or(0),
            // Each pass costs the body plus the head itself.
            Self::Loop { max_iter, body } => (max_iter + 1) * (body.step_budget() + 1),
            Self::Gate(rest) => 1 + rest.step_budget(),
            // A gate may poll several times before its tasks settle, and every
            // poll is a super-step.
            Self::Spawned { tasks, .. } => 2 + (*tasks as u64) + 8,
            // The child runs inside one activation of the parent, so it costs
            // the parent a single step regardless of its own size.
            Self::Nested(_) => 2,
        }
    }

    /// The ids of every approval gate this shape will contain, in the order
    /// [`Builder`] hands ids out.
    ///
    /// A caller needs these up front to pre-approve a run or to resume one, and
    /// recomputing them by walking the built graph would just be re-deriving
    /// what the builder already knew.
    #[must_use]
    pub fn gate_ids(&self) -> Vec<String> {
        let mut builder = Builder::new();
        builder.build(self);
        builder.gates
    }
}

/// Builds the nodes and edges of a graph, handing out unique ids as it goes.
struct Builder {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    next_id: usize,
    /// Ids of the approval gates emitted, in creation order.
    gates: Vec<String>,
}

/// The entry and exit node ids of a built subgraph.
struct Span {
    entry: String,
    exit: String,
}

impl Builder {
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            next_id: 0,
            gates: Vec::new(),
        }
    }

    /// Adds a node with a fresh id and returns that id.
    fn add(&mut self, kind: NodeKind, config: Value) -> String {
        let id = format!("n{}", self.next_id);
        self.next_id += 1;
        self.nodes.push(Node {
            id: id.clone(),
            kind,
            type_version: 1,
            name: id.clone(),
            config,
            ports: vec![],
            position: None,
        });
        id
    }

    /// Adds a passthrough node — the neutral filler this generator builds
    /// linear runs from. `output_parser` with no config emits its input
    /// unchanged and consults no capability.
    fn passthrough(&mut self) -> String {
        self.add(NodeKind::OutputParser, Value::Null)
    }

    fn connect(&mut self, from: &str, port: &str, to: &str) {
        self.edges.push(Edge {
            from_node: from.to_string(),
            from_port: port.to_string(),
            to_node: to.to_string(),
            to_port: "main".to_string(),
        });
    }

    /// Emits `shape` and returns its entry/exit ids.
    fn build(&mut self, shape: &Shape) -> Span {
        match shape {
            Shape::Linear(n) => {
                let entry = self.passthrough();
                let mut exit = entry.clone();
                for _ in 0..*n {
                    let next = self.passthrough();
                    self.connect(&exit, "main", &next);
                    exit = next;
                }
                Span { entry, exit }
            }

            Shape::Branch(yes, no) => {
                // The field is absent from the items flowing through, so it
                // resolves falsey and the `false` arm is the one taken. That is
                // deliberate: it means every run exercises barrier relief on
                // the join, which is the interesting path.
                let head = self.add(NodeKind::Condition, json!({ "field": "take_yes" }));
                let join = self.add(NodeKind::Merge, Value::Null);
                for (port, arm) in [("true", yes), ("false", no)] {
                    let span = self.build(arm);
                    self.connect(&head, port, &span.entry);
                    self.connect(&span.exit, "main", &join);
                }
                Span {
                    entry: head,
                    exit: join,
                }
            }

            Shape::Fanout(branches) => {
                let apex = self.passthrough();
                let join = self.add(NodeKind::Merge, Value::Null);
                for (index, branch) in branches.iter().enumerate() {
                    // A `transform` at the head of each branch stamps which
                    // branch the items came down, so a determinism failure
                    // names the branch that diverged rather than just the run.
                    let tag = self.add(NodeKind::Transform, json!({ "set": { "branch": index } }));
                    let span = self.build(branch);
                    // Every branch leaves the apex on the *same* port, which is
                    // what the engine reads as a parallel fan-out rather than a
                    // conditional choice.
                    self.connect(&apex, "main", &tag);
                    self.connect(&tag, "main", &span.entry);
                    self.connect(&span.exit, "main", &join);
                }
                Span {
                    entry: apex,
                    exit: join,
                }
            }

            Shape::Loop { max_iter, body } => {
                // `on_exceeded: continue` so exhausting the cap is a normal
                // exit through `done` rather than an error. A generated graph
                // should only fail when something is actually wrong.
                let head = self.add(
                    NodeKind::Loop,
                    json!({ "max_iterations": max_iter, "on_exceeded": "continue" }),
                );
                let span = self.build(body);
                self.connect(&head, "body", &span.entry);
                self.connect(&span.exit, "main", &head); // the back-edge
                let out = self.passthrough();
                self.connect(&head, "done", &out);
                Span {
                    entry: head,
                    exit: out,
                }
            }

            Shape::Spawned { tasks, release, n } => {
                let apex = self.passthrough();
                let mut sources = Vec::new();
                for index in 0..*tasks {
                    let spawn = self.add(
                        NodeKind::Spawn,
                        json!({ "target": "tool", "slug": format!("gen.task{index}") }),
                    );
                    self.connect(&apex, "main", &spawn);
                    sources.push(spawn);
                }
                let mut config = json!({
                    "from": sources,
                    "release": release,
                    // Poll fast: these runs are in-process and the interval is
                    // pure latency in a test.
                    "poll_interval_ms": 1,
                    // Settle for what arrived rather than failing, so a release
                    // policy that cannot be met is not reported as a bug.
                    "on_timeout": "partial",
                });
                if matches!(*release, "first_n" | "quorum") {
                    config["n"] = json!((*n).clamp(1, (*tasks).max(1)));
                }
                let gate = self.add(NodeKind::Gate, config);
                for source in &sources {
                    self.connect(source, "main", &gate);
                }
                Span {
                    entry: apex,
                    exit: gate,
                }
            }

            Shape::Nested(inner) => {
                // The child is a complete graph in its own right, built by a
                // fresh builder so its ids live in their own space — exactly the
                // separation that makes a child's gate ids need namespacing.
                let child = graph_of(inner);
                let node = self.add(
                    NodeKind::SubWorkflow,
                    json!({ "workflow": serde_json::to_value(&child).expect("child graph") }),
                );
                Span {
                    entry: node.clone(),
                    exit: node,
                }
            }

            Shape::Gate(rest) => {
                let gate = self.add(NodeKind::OutputParser, json!({ "requires_approval": true }));
                self.gates.push(gate.clone());
                let span = self.build(rest);
                self.connect(&gate, "main", &span.entry);
                Span {
                    entry: gate,
                    exit: span.exit,
                }
            }
        }
    }
}

/// Compiles a [`Shape`] into a runnable [`WorkflowGraph`].
///
/// The graph gets a trigger wired to the shape's entry, and a `recursion_limit`
/// derived from the shape rather than left to the default — a generated graph
/// that legitimately needs many super-steps should not be failed by a budget
/// that has nothing to do with what is being tested.
#[must_use]
pub fn graph_of(shape: &Shape) -> WorkflowGraph {
    let mut builder = Builder::new();
    let span = builder.build(shape);
    let trigger = Node {
        id: "trigger".to_string(),
        kind: NodeKind::Trigger,
        type_version: 1,
        name: "trigger".to_string(),
        config: json!({ "recursion_limit": shape.step_budget() + 8 }),
        ports: vec![],
        position: None,
    };
    builder.edges.push(Edge {
        from_node: "trigger".to_string(),
        from_port: "main".to_string(),
        to_node: span.entry,
        to_port: "main".to_string(),
    });
    let mut nodes = vec![trigger];
    nodes.extend(builder.nodes);
    WorkflowGraph {
        name: "generated".to_string(),
        nodes,
        edges: builder.edges,
        ..Default::default()
    }
}

/// A strategy for shapes, bounded by nesting `depth`.
///
/// Arities are kept small (branch fan-out of 2–3, loops of 1–3 passes) on
/// purpose: property tests find bugs through *structural variety*, not through
/// size, and small counterexamples are the ones a human can read once proptest
/// has shrunk them.
pub fn arb_shape(depth: u32) -> impl Strategy<Value = Shape> {
    let leaf = prop_oneof![
        (0usize..3).prop_map(Shape::Linear),
        // A leaf that spawns: it needs no nesting to be interesting, and putting
        // it here means the async pair turns up at every depth rather than only
        // near the root.
        (1usize..4, 1usize..4).prop_map(|(tasks, n)| Shape::Spawned {
            tasks,
            release: "all",
            n,
        }),
    ];
    leaf.prop_recursive(depth, 32, 4, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone())
                .prop_map(|(a, b)| Shape::Branch(Box::new(a), Box::new(b))),
            prop::collection::vec(inner.clone(), 2..4).prop_map(Shape::Fanout),
            (1u64..4, inner.clone()).prop_map(|(max_iter, body)| Shape::Loop {
                max_iter,
                body: Box::new(body)
            }),
            inner.prop_map(|child| Shape::Nested(Box::new(child))),
        ]
    })
}

/// A strategy over every release policy, for shapes whose point is the gate.
///
/// Kept separate from [`arb_shape`], which pins `release: "all"`: a property
/// about *what a run computes* wants the policy that always collects
/// everything, while a property about *the policies themselves* wants the
/// variety. Mixing them would make the first kind of property
/// nondeterministic for uninteresting reasons.
pub fn arb_spawned_shape() -> impl Strategy<Value = Shape> {
    (1usize..5, 0usize..5, 1usize..4).prop_map(|(tasks, policy, n)| Shape::Spawned {
        tasks,
        release: ["all", "any", "first_n", "quorum", "timeout_partial"][policy],
        n,
    })
}

/// A strategy for whole graphs, at the default nesting depth.
pub fn arb_workflow_graph() -> impl Strategy<Value = WorkflowGraph> {
    arb_shape(3).prop_map(|shape| graph_of(&shape))
}

/// Like [`arb_shape`], but every generated shape contains **at least one**
/// approval gate, so every run it produces suspends.
///
/// Kept separate from [`arb_shape`] rather than folded in as one more variant:
/// a strategy that only *sometimes* produces a gate would spend most of its
/// cases not testing suspension at all, and the properties about running to
/// completion want graphs that reliably do not pause.
pub fn arb_gated_shape(depth: u32) -> impl Strategy<Value = Shape> {
    arb_shape(depth).prop_map(|inner| Shape::Gate(Box::new(inner)))
}
