#[test]
fn schedule_trigger_maps_a_fixed_unit_interval_rule() {
    let mut warnings = Vec::new();
    let cfg = trigger_config(
        "schedule",
        &json!({
            "rule": { "interval": [{ "field": "hours", "hoursInterval": 2 }] }
        }),
        &mut warnings,
        "ScheduleTrigger",
    );
    assert_eq!(
        cfg["schedule"],
        json!({ "kind": "every", "every_ms": 7200000.0 })
    );
}

#[test]
fn unrecognized_schedule_shape_warns_instead_of_guessing() {
    let mut warnings = Vec::new();
    let cfg = trigger_config(
        "schedule",
        &json!({ "rule": { "interval": [{ "field": "weekday", "weekday": 1 }] } }),
        &mut warnings,
        "Weekly",
    );
    assert!(cfg.get("schedule").is_none());
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("Weekly") && w.contains("could not be translated"))
    );
}

#[test]
fn multiple_schedule_intervals_warn_instead_of_dropping_cadences() {
    let mut warnings = Vec::new();
    let cfg = trigger_config(
        "schedule",
        &json!({ "rule": { "interval": [
            { "field": "hours", "hoursInterval": 2 },
            { "field": "hours", "hoursInterval": 6 }
        ] } }),
        &mut warnings,
        "Several cadences",
    );
    assert!(cfg.get("schedule").is_none());
    assert!(warnings.iter().any(|warning| {
        warning.contains("Several cadences") && warning.contains("could not be translated")
    }));
}

#[test]
fn non_positive_or_sub_millisecond_intervals_are_not_scheduled() {
    for value in [-1.0, 0.0, 0.000_1, f64::MAX] {
        let mut warnings = Vec::new();
        let cfg = trigger_config(
            "schedule",
            &json!({ "unit": "seconds", "value": value }),
            &mut warnings,
            "Invalid interval",
        );
        assert!(cfg.get("schedule").is_none(), "value={value}: {cfg}");
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("could not be translated"))
        );
    }
}
