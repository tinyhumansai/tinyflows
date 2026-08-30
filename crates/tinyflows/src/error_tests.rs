use super::*;

#[test]
fn validation_error_display() {
    assert_eq!(
        ValidationError::MissingTrigger.to_string(),
        "workflow has no trigger node"
    );
    assert_eq!(
        ValidationError::MultipleTriggers(vec!["t1".to_string(), "t2".to_string()]).to_string(),
        "workflow has multiple trigger nodes: [\"t1\", \"t2\"]"
    );
    assert_eq!(
        ValidationError::UnknownNode("ghost".to_string()).to_string(),
        "edge references unknown node id: ghost"
    );
    assert_eq!(
        ValidationError::DuplicateNodeId("dup".to_string()).to_string(),
        "duplicate node id: dup"
    );
    assert_eq!(
        ValidationError::IllegalCycle("loop".to_string()).to_string(),
        "illegal cycle detected involving node: loop"
    );
    assert_eq!(
        ValidationError::InvalidNodeConfig {
            node: "n1".to_string(),
            reason: "missing url".to_string(),
        }
        .to_string(),
        "invalid config for node n1: missing url"
    );
    assert_eq!(
        ValidationError::MissingErrorRoute("n1".to_string()).to_string(),
        "node n1 has on_error=\"route\" but no outgoing edge on its `error` port"
    );
    assert_eq!(
        ValidationError::DuplicateEdge {
            from_node: "a".to_string(),
            from_port: "main".to_string(),
            to_node: "b".to_string(),
            to_port: "main".to_string(),
        }
        .to_string(),
        "duplicate edge: a.main -> b.main"
    );
    assert_eq!(
        ValidationError::InvalidOnError {
            node: "n1".to_string(),
            value: "explode".to_string(),
        }
        .to_string(),
        "node n1 has unknown on_error value: \"explode\""
    );
    assert_eq!(
        ValidationError::SchemaVersionTooNew {
            found: 5,
            supported: 1,
        }
        .to_string(),
        "schema_version 5 is newer than this crate supports (max 1); \
             upgrade tinyflows to load this graph"
    );
    assert_eq!(
        ValidationError::InvalidConditionRouting {
            node: "gate".to_string(),
            from_port: "main".to_string(),
        }
        .to_string(),
        "condition node gate has an outgoing edge with from_port \"main\" — condition \
             edges must emit on from_port \"true\" or \"false\" (the branch label belongs on \
             from_port, not to_port; routing is keyed exclusively on from_port)"
    );
}

#[test]
fn declared_input_validation_error_display_and_anchors() {
    let dup = ValidationError::DuplicateInputName("repo".to_string());
    assert_eq!(dup.to_string(), "duplicate workflow input name: repo");
    assert_eq!(dup.code(), "duplicate_input_name");
    assert_eq!(dup.input_name(), Some("repo"));
    assert_eq!(dup.node_id(), None);

    assert_eq!(
        ValidationError::InvalidInputName("repo-url".to_string()).to_string(),
        "invalid workflow input name \"repo-url\" — names must match \
             [A-Za-z_][A-Za-z0-9_]* so `=inputs.<name>` can address them"
    );
    assert_eq!(
        ValidationError::InputDefaultTypeMismatch {
            name: "depth".to_string(),
            expected: "number",
        }
        .to_string(),
        "workflow input \"depth\" has a default that is not of its declared type number"
    );
    assert_eq!(
        ValidationError::RequiredInputWithDefault("repo".to_string()).to_string(),
        "workflow input \"repo\" is both required and has a default; \
             a default makes it optional"
    );

    // Node-anchored errors are not input-anchored, and vice versa.
    assert_eq!(
        ValidationError::UnknownNode("ghost".to_string()).input_name(),
        None
    );
}

#[test]
fn engine_error_display() {
    assert_eq!(
        EngineError::Unimplemented("checkpoint replay").to_string(),
        "not yet implemented: checkpoint replay"
    );
    assert_eq!(
        EngineError::Capability("http timed out".to_string()).to_string(),
        "capability error: http timed out"
    );
    assert_eq!(
        EngineError::Validation(ValidationError::MissingTrigger).to_string(),
        "validation failed: workflow has no trigger node"
    );
    assert_eq!(
        EngineError::Input(crate::model::InputError::Missing("repo".to_string())).to_string(),
        "input error: workflow input \"repo\" is required but was not supplied"
    );
}

#[test]
fn input_error_lifts_into_engine_error() {
    let engine: EngineError = crate::model::InputError::Unknown("reop".to_string()).into();
    match engine {
        EngineError::Input(inner) => assert_eq!(inner.input_name(), "reop"),
        other => panic!("expected lifted input error, got {other:?}"),
    }
}

#[test]
fn validation_error_converts_into_engine_error() {
    let engine: EngineError = ValidationError::MissingTrigger.into();
    assert!(matches!(
        engine,
        EngineError::Validation(ValidationError::MissingTrigger)
    ));
}

#[test]
fn question_mark_operator_lifts_validation_error() {
    fn inner() -> Result<()> {
        Err(ValidationError::DuplicateNodeId("dup".to_string()))?;
        Ok(())
    }
    match inner() {
        Err(EngineError::Validation(ValidationError::DuplicateNodeId(id))) => {
            assert_eq!(id, "dup");
        }
        other => panic!("expected lifted validation error, got {other:?}"),
    }
}

#[test]
fn validation_error_is_comparable_and_cloneable() {
    let err = ValidationError::UnknownNode("x".to_string());
    assert_eq!(err.clone(), err);
    assert_ne!(err, ValidationError::MissingTrigger);
}
