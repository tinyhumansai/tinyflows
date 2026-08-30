//! Tests for the tool contracts.
//!
//! These are the descriptors a host hands a model, so what is worth asserting
//! is that they are complete, self-consistent, and stable.

use super::*;

#[test]
fn every_tool_has_a_name_summary_and_description() {
    for tool in all_tools() {
        assert!(!tool.name.is_empty());
        assert!(!tool.summary.is_empty(), "{} has no summary", tool.name);
        assert!(
            tool.description.len() > tool.summary.len(),
            "{}'s description should say more than its summary",
            tool.name
        );
    }
}

#[test]
fn every_input_schema_is_a_closed_object() {
    // `additionalProperties: false` is what turns a model's invented argument
    // into a validation error the host can report, rather than a silently
    // ignored one.
    for tool in all_tools() {
        assert_eq!(
            tool.input_schema["type"], "object",
            "{} should take an object",
            tool.name
        );
        assert_eq!(
            tool.input_schema["additionalProperties"], false,
            "{} should refuse unknown arguments",
            tool.name
        );
        assert!(
            tool.input_schema["properties"].is_object(),
            "{} should declare its properties",
            tool.name
        );
    }
}

#[test]
fn every_required_argument_is_declared_as_a_property() {
    for tool in all_tools() {
        let properties = tool.input_schema["properties"]
            .as_object()
            .expect("properties");
        for required in tool.input_schema["required"]
            .as_array()
            .expect("required is an array")
        {
            let name = required.as_str().expect("required names are strings");
            assert!(
                properties.contains_key(name),
                "{} requires {name:?} but does not declare it",
                tool.name
            );
        }
    }
}

#[test]
fn names_are_unique_and_namespaced() {
    let tools = all_tools();
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    names.sort_unstable();
    let unique = names.len();
    names.dedup();
    assert_eq!(unique, names.len(), "tool names must be unique");

    for tool in &tools {
        assert!(
            tool.name.starts_with("flow_test.") || tool.name.starts_with("flow_debug."),
            "{} should be namespaced",
            tool.name
        );
    }
}

#[test]
fn every_debug_tool_takes_a_session_id() {
    for tool in all_tools()
        .iter()
        .filter(|t| t.name.starts_with("flow_debug."))
    {
        // Except the one that creates the session.
        if tool.name == "flow_debug.start" {
            continue;
        }
        assert!(
            tool.input_schema["properties"].get("session_id").is_some(),
            "{} should take a session_id",
            tool.name
        );
    }
}

#[test]
fn read_only_tools_are_not_marked_mutating() {
    // A host may gate mutating calls behind confirmation, so the flag has to be
    // right rather than merely present.
    let read_only = [
        "flow_test.run",
        "flow_test.trace",
        "flow_test.node",
        "flow_debug.wait",
        "flow_debug.status",
        "flow_debug.trace",
    ];
    for name in read_only {
        let tool = tool_for(name).expect("tool exists");
        assert!(!tool.mutating, "{name} does not change session state");
    }
    for name in ["flow_debug.start", "flow_debug.release", "flow_debug.stop"] {
        let tool = tool_for(name).expect("tool exists");
        assert!(tool.mutating, "{name} changes session state");
    }
}

#[test]
fn tool_for_finds_by_name_and_reports_a_miss() {
    assert!(tool_for("flow_test.run").is_some());
    assert!(tool_for("nope").is_none());
}

#[test]
fn contracts_round_trip_through_json() {
    let tools = all_tools();
    let encoded = serde_json::to_string(&tools).expect("serialize");
    let decoded: Vec<ToolContract> = serde_json::from_str(&encoded).expect("deserialize");
    assert_eq!(tools, decoded);
}
