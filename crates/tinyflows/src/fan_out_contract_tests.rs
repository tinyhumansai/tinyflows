use super::*;

#[test]
fn every_mapping_kind_advertises_the_fan_out_knobs() {
    for (kind, _) in FAN_OUT_KINDS {
        let c = contract_for(kind).expect("contract");
        for field in ["execution", "concurrency", "on_item_error"] {
            assert!(
                c.config_fields.iter().any(|f| f.name == field),
                "{kind} should advertise `{field}`"
            );
        }
    }
}

#[test]
fn kinds_that_cannot_map_do_not_advertise_them() {
    // The contract and the validator must agree on which kinds fan out;
    // advertising a key that validation rejects would be worse than silence.
    for kind in [
        "trigger",
        "condition",
        "switch",
        "merge",
        "transform",
        "code",
        "void",
    ] {
        let c = contract_for(kind).expect("contract");
        assert!(
            !c.config_fields.iter().any(|f| f.name == "concurrency"),
            "{kind} must not advertise `concurrency`"
        );
    }
}

#[test]
fn the_execution_default_is_stated_per_kind() {
    let doc = |kind: &str| {
        contract_for(kind)
            .expect("contract")
            .config_fields
            .iter()
            .find(|f| f.name == "execution")
            .expect("execution field")
            .description
            .clone()
    };
    // An author needs to know that `agent` must opt in but `tool_call` need not.
    assert!(doc("agent").contains("\"once\""), "{}", doc("agent"));
    assert!(
        doc("tool_call").contains("\"per_item\""),
        "{}",
        doc("tool_call")
    );
}
