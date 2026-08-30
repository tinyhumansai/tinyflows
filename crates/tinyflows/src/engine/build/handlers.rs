use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn register_handlers(
    mut builder: GraphBuilder<Value, Value>,
    graph: &crate::model::WorkflowGraph,
    capabilities: &Capabilities,
    observer: &Arc<dyn RunObserver>,
    steps: &Arc<Mutex<Vec<ExecutionStep>>>,
    terminal_error: &Arc<Mutex<Option<EngineError>>>,
    token: &CancellationToken,
    node_timeout: Option<std::time::Duration>,
    loop_edges: &std::collections::HashSet<(String, String)>,
    interceptor: Option<&Arc<dyn StepInterceptor>>,
) -> GraphBuilder<Value, Value> {
    let is_back_edge = |edge: &crate::model::Edge| {
        loop_edges.contains(&(edge.from_node.clone(), edge.to_node.clone()))
    };

    // The graph's own agent registry, shared by every node handler. An `agent`
    // node resolves its `agent_ref` here first (see `crate::nodes::integration`),
    // falling back to the harness's registry. Held behind an `Arc` and cloned per
    // handler rather than seeded into run state, which is checkpointed on every
    // super-step: the registry is static for the run, so serializing it into each
    // checkpoint to read it back in one node would be pure waste.
    let agents: Arc<Vec<crate::model::AgentDefinition>> = Arc::new(graph.agents.clone());

    for node in &graph.nodes {
        let node = node.clone();
        // Incoming edges as `(predecessor id, edge from_port)` pairs, so
        // `collect_input` can gather each predecessor's items only from the port
        // it actually emitted on (see `collect_input`).
        let incoming: Vec<(String, String)> = graph
            .edges
            .iter()
            .filter(|e| e.to_node == node.id && !is_back_edge(e))
            .map(|e| (e.from_node.clone(), e.from_port.clone()))
            .collect();
        // The same, for the node's back-edges only. A loop head is entered once
        // through its forward edges (the seed) and re-entered through these.
        // Keeping the two apart is what makes an iteration see the *body's*
        // output rather than the trigger's: `collect_input` gathers from every
        // predecessor whose slot still port-matches, and the seeding
        // predecessor's slot never goes away, so a single merged list would
        // silently re-deliver the seed on every pass.
        let back_incoming: Vec<(String, String)> = graph
            .edges
            .iter()
            .filter(|e| e.to_node == node.id && is_back_edge(e))
            .map(|e| (e.from_node.clone(), e.from_port.clone()))
            .collect();
        let caps = capabilities.clone();
        let agents = agents.clone();
        let observer = observer.clone();
        let interceptor = interceptor.cloned();
        let steps = steps.clone();
        let terminal_error = terminal_error.clone();
        let token = token.clone();
        let is_trigger = node.kind == NodeKind::Trigger;
        // How this node drives its successors once it has an update.
        let routing = handler_routing(graph, &node.id);
        // Successors on the emitted port, needed only inside a lane: `Plain`
        // routing normally rides static edges, but a lane has to re-schedule
        // every successor as a `Send`, so it needs the target list explicitly.
        let plain_targets_by_port = outgoing_by_port(graph, &node.id);
        let plain_targets: Vec<String> = plain_targets_by_port
            .iter()
            .flat_map(|(_, targets)| targets.iter().cloned())
            .collect();
        // Which successors end a lane. Routing to one of these is a plain
        // activation, so the lanes converge on it instead of each running their
        // own copy.
        let gather_nodes: std::collections::HashSet<String> = graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Gather)
            .map(|n| n.id.clone())
            .collect();
        // Whether the node has an outgoing edge on the `error` port. A denied
        // approval gate (see the resume-deny path below) routes its error item
        // there when present, and fails the run when absent.
        let has_error_edge = graph
            .edges
            .iter()
            .any(|e| e.from_node == node.id && e.from_port == "error");

        let handler = activation::HandlerData {
            node,
            incoming,
            back_incoming,
            caps,
            agents,
            observer,
            steps,
            terminal_error,
            token,
            routing,
            plain_targets,
            plain_targets_by_port,
            gather_nodes,
            has_error_edge,
            is_trigger,
            node_timeout,
            interceptor,
        };
        let node_id = handler.node.id.clone();
        builder = builder.add_node(node_id, move |state: Value, ctx| {
            let handler = handler.clone();
            async move { handler.execute(state, ctx).await }
        });
    }
    builder
}
