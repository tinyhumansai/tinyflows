use super::*;

impl<State, Update> GraphBuilder<State, Update>
where
    State: Clone + Send + Sync + 'static,
    Update: Send + 'static,
{
    /// Creates an empty builder with a generated graph id and a default
    /// recursion limit of 50. A reducer must be set before [`Self::compile`].
    pub fn new() -> Self {
        Self {
            graph_id: GraphId::new(format!("graph-{}", crate::graph::ids::next_seq())),
            name: None,
            nodes: HashMap::new(),
            edges: HashMap::new(),
            branches: HashMap::new(),
            command_nodes: HashSet::new(),
            waiting: HashMap::new(),
            barrier_reliefs: Vec::new(),
            reducer: None,
            recursion_limit: 50,
            parallel: false,
            max_concurrency: None,
            node_concurrency: HashMap::new(),
            node_timeout: None,
            node_meta: HashMap::new(),
        }
    }

    /// Applies a bundle of [`GraphDefaults`] in one call. Only the `Some` fields
    /// override the builder's current configuration, so this composes with
    /// explicit `with_*` calls regardless of ordering.
    pub fn set_defaults(mut self, defaults: GraphDefaults) -> Self {
        if let Some(limit) = defaults.recursion_limit {
            self.recursion_limit = limit;
        }
        if let Some(parallel) = defaults.parallel {
            self.parallel = parallel;
        }
        if let Some(max) = defaults.max_concurrency {
            self.max_concurrency = Some(max);
        }
        if let Some(timeout) = defaults.node_timeout {
            self.node_timeout = Some(timeout);
        }
        self
    }

    /// Bounds the number of branches run concurrently within a single superstep
    /// (only meaningful with [`Self::with_parallel`] enabled). The executor runs
    /// the active set in chunks of at most `n` futures, so at most `n` node
    /// handlers are in flight at once. `0` is treated as unbounded.
    pub fn with_max_concurrency(mut self, n: usize) -> Self {
        self.max_concurrency = (n > 0).then_some(n);
        self
    }

    /// Bounds how many activations of **one node** may run concurrently within a
    /// step, independently of the graph-wide [`Self::with_max_concurrency`].
    ///
    /// Only meaningful for a node that can be activated more than once in the
    /// same superstep — i.e. one scheduled through `Send` packets, where a fan-out
    /// of N items becomes N activations of the same node. The graph-wide bound
    /// cannot express "let this one node run 4 at a time" without also throttling
    /// every unrelated branch in the step.
    ///
    /// Both bounds apply: a branch starts only when the step is below the global
    /// limit *and* its node is below this one. `0` removes the per-node bound.
    pub fn with_node_concurrency(mut self, node: impl Into<NodeId>, n: usize) -> Self {
        let node = node.into();
        if n == 0 {
            self.node_concurrency.remove(&node);
        } else {
            self.node_concurrency.insert(node, n);
        }
        self
    }

    /// Sets a default wall-clock timeout applied to every node handler. A node
    /// whose future does not resolve within `timeout` fails the run with
    /// [`crate::GraphError::Timeout`].
    pub fn with_node_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.node_timeout = Some(timeout);
        self
    }

    /// Enables or disables concurrent execution of the active node set within a
    /// superstep. Defaults to `false` (sequential), which preserves the exact
    /// milestone-1 ordering and semantics.
    ///
    /// When enabled, a superstep with more than one active node runs every
    /// branch concurrently via `futures::future::join_all`. Each branch gets its
    /// own cloned `State` snapshot (`State: Clone`) and a distinct
    /// [`ForkId`] on its [`NodeContext`]. Branch results are still folded into
    /// the reducer in deterministic active-set order at the step boundary, so a
    /// downstream node always observes the same merged state regardless of which
    /// branch finished first. See [`crate::graph::CompiledGraph`] for the full
    /// concurrency and interrupt semantics.
    pub fn with_parallel(mut self, parallel: bool) -> Self {
        self.parallel = parallel;
        self
    }

    /// Overrides the graph id.
    pub fn with_graph_id(mut self, id: impl Into<GraphId>) -> Self {
        self.graph_id = id.into();
        self
    }

    /// Sets an optional human-readable graph name surfaced by the topology
    /// export. The `graph_id` remains the stable identifier; the name is purely
    /// descriptive (e.g. for diagrams a model authored).
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Overrides the recursion limit (max number of supersteps).
    pub fn with_recursion_limit(mut self, limit: usize) -> Self {
        self.recursion_limit = limit;
        self
    }

    /// Sets the state reducer used to merge partial updates at step boundaries.
    pub fn set_reducer<R>(mut self, reducer: R) -> Self
    where
        R: StateReducer<State, Update> + 'static,
    {
        self.reducer = Some(Arc::new(reducer));
        self
    }

    /// Adds an async node returning a [`NodeResult`].
    pub fn add_node<F, Fut>(mut self, id: impl Into<NodeId>, handler: F) -> Self
    where
        F: Fn(State, NodeContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<NodeResult<Update>>> + Send + 'static,
    {
        let id = id.into();
        self.nodes.insert(
            id.clone(),
            BuilderNode {
                id,
                handler: Arc::new(move |state, ctx| Box::pin(handler(state, ctx))),
            },
        );
        self
    }

    /// Adds a direct edge `from -> to`. Use [`START`]/[`END`] for the virtual
    /// entry/terminal nodes.
    pub fn add_edge(mut self, from: impl Into<NodeId>, to: impl Into<NodeId>) -> Self {
        self.edges.insert(from.into(), to.into());
        self
    }

    /// Adds a chain of direct edges over the given nodes: `add_sequence([a, b,
    /// c])` is equivalent to `add_edge(a, b).add_edge(b, c)`. The nodes must
    /// already have been added with [`Self::add_node`]; this only wires edges. A
    /// sequence of fewer than two nodes adds no edges.
    pub fn add_sequence<I, N>(mut self, nodes: I) -> Self
    where
        I: IntoIterator<Item = N>,
        N: Into<NodeId>,
    {
        let nodes: Vec<NodeId> = nodes.into_iter().map(Into::into).collect();
        for pair in nodes.windows(2) {
            self.edges.insert(pair[0].clone(), pair[1].clone());
        }
        self
    }

    /// Adds a barrier/waiting edge `from -> to`: like [`Self::add_edge`] but `to`
    /// only activates once *all* of its registered predecessors (every `from`
    /// declared via `add_waiting_edge`) have completed — possibly across
    /// different supersteps. This is the join/fan-in primitive for diamond
    /// topologies where several branches must finish before a synthesis node
    /// runs. Calling it repeatedly with the same `to` accumulates the required
    /// predecessor set.
    pub fn add_waiting_edge(mut self, from: impl Into<NodeId>, to: impl Into<NodeId>) -> Self {
        let from = from.into();
        let to = to.into();
        self.edges.insert(from.clone(), to.clone());
        self.waiting.entry(to).or_default().insert(from);
        self
    }

    /// Registers a barrier relief for a mixed fan-in barrier.
    ///
    /// When `source` completes a superstep without routing to `relief_node`
    /// — because `relief_node` sits behind a conditional branch `source` did
    /// not take this run — this records a phantom arrival of `relief_node`
    /// at `barrier_node`'s waiting-edge barrier (registered separately via
    /// [`Self::add_waiting_edge`]), so an all-waiting merge downstream of a
    /// mixed fan-in (one unconditionally reachable predecessor plus one
    /// reachable only via a conditional branch) can still clear on its
    /// remaining predecessors instead of deadlocking on a branch that will
    /// never run.
    ///
    /// `relief_node` must be one of `barrier_node`'s registered waiting
    /// predecessors; a relief for a barrier with no matching waiting
    /// registration is a no-op at execution time.
    pub fn add_barrier_relief(
        mut self,
        source: impl Into<NodeId>,
        relief_node: impl Into<NodeId>,
        barrier_node: impl Into<NodeId>,
    ) -> Self {
        self.barrier_reliefs.push(BarrierRelief {
            source: source.into(),
            relief_node: relief_node.into(),
            barrier_node: barrier_node.into(),
        });
        self
    }

    /// Sets the entry node, i.e. `add_edge(START, node)`.
    pub fn set_entry(self, node: impl Into<NodeId>) -> Self {
        self.add_edge(START, node)
    }

    /// Marks `node` as terminal, i.e. `add_edge(node, END)`.
    pub fn set_finish(self, node: impl Into<NodeId>) -> Self {
        self.add_edge(node, END)
    }

    /// Adds conditional edges: a router closure mapped against a label table.
    ///
    /// Both the router's return value and the route-table labels are
    /// `impl ToString`, so a user-defined route enum that implements `Display`
    /// (or the [`Route`] newtype) can be used directly without manual
    /// `.to_string()` calls. Plain `&str`/`String` labels still work unchanged —
    /// the label is resolved against the table by its string form at the step
    /// boundary.
    pub fn add_conditional_edges<F, R, I, K, V>(
        mut self,
        from: impl Into<NodeId>,
        router: F,
        routes: I,
    ) -> Self
    where
        F: Fn(&State) -> R + Send + Sync + 'static,
        R: ToString,
        I: IntoIterator<Item = (K, V)>,
        K: ToString,
        V: Into<NodeId>,
    {
        let routes = routes
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.into()))
            .collect();
        self.branches.insert(
            from.into(),
            Branch {
                router: Arc::new(move |state| router(state).to_string()),
                routes,
            },
        );
        self
    }

    /// Declares that `node` routes exclusively via [`crate::graph::Command`]
    /// `goto` (not static or conditional edges). Compile rejects nodes that mix
    /// command routing with static/conditional edges.
    pub fn mark_command_routing(mut self, node: impl Into<NodeId>) -> Self {
        self.command_nodes.insert(node.into());
        self
    }

    /// Records the declared `goto` destination hints for a command-routing node.
    ///
    /// These are advisory only — the runtime always resolves the real successor
    /// from the [`crate::graph::Command`] a node emits — but they let the
    /// [topology export](crate::graph::export) draw and validate the set of
    /// nodes a command node may jump to. Implies [`Self::mark_command_routing`].
    pub fn with_command_destinations<I, N>(
        mut self,
        node: impl Into<NodeId>,
        destinations: I,
    ) -> Self
    where
        I: IntoIterator<Item = N>,
        N: Into<NodeId>,
    {
        let node = node.into();
        self.command_nodes.insert(node.clone());
        let dests = destinations.into_iter().map(Into::into).collect();
        self.node_meta.entry(node).or_default().command_destinations = dests;
        self
    }

    /// Records destinations for a command node that fans out to **all** of
    /// them, unconditionally, every time it runs.
    ///
    /// Like [`Self::with_command_destinations`], but additionally promises
    /// there is no runtime choice among the destinations. Barrier relief relies
    /// on that promise to walk forward through this node when deciding whether
    /// a conditional branch was taken; without it, relief treats the node as an
    /// unresolvable decision point, concludes the branch was untaken, and
    /// clears a downstream barrier before its real predecessors have run.
    ///
    /// Only use this where every destination genuinely always runs. A node that
    /// picks between ports must use [`Self::with_command_destinations`].
    pub fn with_unconditional_fanout<I, N>(
        mut self,
        node: impl Into<NodeId>,
        destinations: I,
    ) -> Self
    where
        I: IntoIterator<Item = N>,
        N: Into<NodeId>,
    {
        let node = node.into();
        self = self.with_command_destinations(node.clone(), destinations);
        self.node_meta.entry(node).or_default().command_fanout = true;
        self
    }

    /// Sets a human-readable kind for `node` (e.g. `model`, `tool`, `subgraph`)
    /// surfaced as [`crate::graph::NodeInfo::kind`] in the export.
    pub fn with_node_kind(mut self, node: impl Into<NodeId>, kind: impl Into<String>) -> Self {
        self.node_meta.entry(node.into()).or_default().kind = Some(kind.into());
        self
    }

    /// Records a sorted, free-form `key=value` annotation on `node`, carried
    /// verbatim into [`crate::graph::NodeInfo::metadata`].
    pub fn with_node_metadata(
        mut self,
        node: impl Into<NodeId>,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.node_meta
            .entry(node.into())
            .or_default()
            .metadata
            .insert(key.into(), value.into());
        self
    }

    /// Marks `node` as a subgraph-embedding node for the export (an introspection
    /// marker only; it does not change how the node executes).
    pub fn mark_subgraph(mut self, node: impl Into<NodeId>) -> Self {
        self.node_meta.entry(node.into()).or_default().subgraph = true;
        self
    }

    /// Marks `node` as an interrupt point for the export.
    pub fn mark_interrupt(mut self, node: impl Into<NodeId>) -> Self {
        self.node_meta.entry(node.into()).or_default().interrupt = true;
        self
    }

    /// Marks `node` as a deferred join for the export.
    pub fn mark_deferred(mut self, node: impl Into<NodeId>) -> Self {
        self.node_meta.entry(node.into()).or_default().deferred = true;
        self
    }

    /// Validates topology and freezes the graph into a [`CompiledGraph`].
    pub fn compile(self) -> Result<CompiledGraph<State, Update>> {
        if self.reducer.is_none() {
            return Err(GraphError::Validation(
                "no state reducer set; call set_reducer (or GraphBuilder::overwrite)".to_string(),
            ));
        }

        // entry must exist
        let entry = self
            .edges
            .get(&NodeId::from(START))
            .cloned()
            .ok_or(GraphError::MissingStart)?;
        if entry.as_str() == END {
            return Err(GraphError::Validation(
                "START cannot route directly to END".to_string(),
            ));
        }
        self.require_node(&entry)?;

        // static edges
        for (from, to) in &self.edges {
            // Check reserved virtual nodes before ordinary membership. START
            // and END intentionally are not present in `nodes`, so doing the
            // membership lookup first hides these actionable topology errors
            // behind a generic MissingNode result.
            if to.as_str() == START {
                return Err(GraphError::Validation(
                    "START cannot be an edge target".to_string(),
                ));
            }
            if from.as_str() == END {
                return Err(GraphError::Validation(
                    "END cannot be an edge source".to_string(),
                ));
            }
            if from.as_str() != START {
                self.require_node(from)?;
            }
            if to.as_str() != END {
                self.require_node(to)?;
            }
        }

        // conditional edges
        for (from, branch) in &self.branches {
            self.require_node(from)?;
            if self.edges.contains_key(from) {
                return Err(GraphError::Validation(format!(
                    "node `{from}` has both a static edge and conditional edges"
                )));
            }
            for target in branch.routes.values() {
                if target.as_str() != END {
                    self.require_node(target)?;
                }
            }
        }

        // barrier/waiting edges: every source and target must exist
        for (to, froms) in &self.waiting {
            self.require_node(to)?;
            for from in froms {
                self.require_node(from)?;
            }
        }

        // command-routing nodes must not also have static/conditional edges
        for node in &self.command_nodes {
            self.require_node(node)?;
            if self.edges.contains_key(node) || self.branches.contains_key(node) {
                return Err(GraphError::Validation(format!(
                    "node `{node}` declares command routing but also has static/conditional edges"
                )));
            }
        }

        let Self {
            graph_id,
            name,
            nodes,
            edges,
            branches,
            command_nodes,
            waiting,
            reducer,
            recursion_limit,
            parallel,
            max_concurrency,
            node_concurrency,
            node_timeout,
            node_meta,
            barrier_reliefs,
        } = self;

        Ok(CompiledGraph::from_parts(
            graph_id,
            name,
            nodes,
            edges,
            branches,
            command_nodes,
            waiting,
            entry,
            reducer.expect("reducer presence checked above"),
            recursion_limit,
            parallel,
            max_concurrency,
            node_concurrency,
            node_timeout,
            node_meta,
            barrier_reliefs,
        ))
    }

    fn require_node(&self, id: &NodeId) -> Result<()> {
        if self.nodes.contains_key(id) {
            Ok(())
        } else {
            Err(GraphError::MissingNode(id.to_string()))
        }
    }
}

impl<State> GraphBuilder<State, State>
where
    State: Clone + Send + Sync + 'static,
{
    /// Creates a builder that uses whole-state overwrite updates — the
    /// milestone-1 default where each node returns the full next state.
    pub fn overwrite() -> Self {
        Self::new().set_reducer(OverwriteStateReducer)
    }
}
