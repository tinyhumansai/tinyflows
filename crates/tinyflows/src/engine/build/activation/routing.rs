|mut update: Value, port: Option<&str>, routed_items: &[Item]| {
    // Only a non-lane activation stamps the node's slot: the
    // stamp is how a loop head tells its own re-entry from a
    // stale arm, and a lane slot is not that.
    if lane.is_none() {
        stamp_activation_step(&mut update, &node.id, ctx.step);
    }

    // Inside a lane, routing carries the lane onward. Every
    // successor is re-scheduled as a `Send` holding this
    // activation's output and the same lane identity, so the
    // whole downstream path runs once per lane rather than
    // once in total.
    //
    // Except a gather: that is where lanes end. A gather is
    // scheduled as a plain activation, and plain activations
    // dedupe by node, so N lanes converge on one gather rather
    // than activating it N times.
    if let Some(lane) = lane.as_ref() {
        let emitted = port.unwrap_or("main");
        let targets: Vec<String> = match &routing {
            HandlerRouting::Plain => plain_targets_by_port
                .iter()
                .find(|(port, _)| port == emitted)
                .map(|(_, targets)| targets.clone())
                .unwrap_or_default(),
            HandlerRouting::FanOut(targets) => targets.clone(),
            HandlerRouting::PortCommand(groups) => groups
                .iter()
                .find(|(p, _)| p == emitted)
                .map(|(_, targets)| targets.clone())
                .unwrap_or_default(),
        };
        let routed: Vec<RouteTarget> = targets
            .into_iter()
            .map(|target| {
                if gather_nodes.contains(&target) {
                    RouteTarget::Node(target.into())
                } else {
                    let envelope =
                        lane_envelope(&lane.origin, lane.index, lane.count, routed_items)
                            .unwrap_or(Value::Null);
                    RouteTarget::Send(crate::graph::Send::new(target, envelope))
                }
            })
            .collect();
        return NodeResult::Command(Command::route(routed).with_update(update));
    }

    match &routing {
        HandlerRouting::Plain => NodeResult::Update(update),
        HandlerRouting::FanOut(targets) => {
            NodeResult::Command(Command::goto(targets.clone()).with_update(update))
        }
        HandlerRouting::PortCommand(groups) => {
            let emitted = port.unwrap_or("main");
            let targets: Vec<String> = groups
                .iter()
                .find(|(p, _)| p == emitted)
                .map(|(_, targets)| targets.clone())
                .unwrap_or_default();
            NodeResult::Command(Command::goto(targets).with_update(update))
        }
    }
}
