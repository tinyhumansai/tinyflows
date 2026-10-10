// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use chrono::{Datelike, TimeZone};
use std::collections::BTreeSet;

// ── 5-field crontab weekdays are POSIX: 0 and 7 = Sunday ────────

fn utc_cron(expr: &str) -> Schedule {
    Schedule::Cron {
        expr: expr.into(),
        tz: Some("UTC".into()),
        active_hours: None,
    }
}

/// Monday 2026-03-02, 00:00 UTC.
fn monday() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 2, 0, 0, 0).unwrap()
}

/// The days a `0 9 * * <dow>` schedule fires on over one week, as POSIX
/// weekday numbers (0 = Sunday … 6 = Saturday).
fn fire_days(dow: &str) -> BTreeSet<u32> {
    let schedule = utc_cron(&format!("0 9 * * {dow}"));
    let week_end = monday() + ChronoDuration::days(7);
    let mut days = BTreeSet::new();
    let mut from = monday();
    loop {
        let next = next_run_for_schedule(&schedule, from)
            .unwrap_or_else(|err| panic!("`0 9 * * {dow}` failed: {err:#}"));
        if next >= week_end {
            return days;
        }
        days.insert(next.weekday().num_days_from_sunday());
        from = next;
    }
}

fn days(posix: &[u32]) -> BTreeSet<u32> {
    posix.iter().copied().collect()
}

#[test]
fn five_means_friday() {
    let next = next_run_for_schedule(&utc_cron("0 16 * * 5"), monday()).unwrap();
    assert_eq!(next, Utc.with_ymd_and_hms(2026, 3, 6, 16, 0, 0).unwrap());
    assert_eq!(next.weekday(), chrono::Weekday::Fri);
}

#[test]
fn zero_and_seven_both_mean_sunday() {
    let sunday = Utc.with_ymd_and_hms(2026, 3, 8, 9, 0, 0).unwrap();
    for expr in ["0 9 * * 0", "0 9 * * 7"] {
        let next = next_run_for_schedule(&utc_cron(expr), monday()).unwrap();
        assert_eq!(next, sunday, "{expr}");
    }
}

#[test]
fn one_to_five_is_monday_to_friday() {
    assert_eq!(fire_days("1-5"), days(&[1, 2, 3, 4, 5]));
    // Friday's run is followed by Monday's, not Saturday's or Sunday's.
    let friday_evening = Utc.with_ymd_and_hms(2026, 3, 6, 10, 0, 0).unwrap();
    let next = next_run_for_schedule(&utc_cron("0 9 * * 1-5"), friday_evening).unwrap();
    assert_eq!(next, Utc.with_ymd_and_hms(2026, 3, 9, 9, 0, 0).unwrap());
}

#[test]
fn steps_lists_and_wildcards() {
    assert_eq!(fire_days("*/2"), days(&[0, 2, 4, 6]));
    assert_eq!(fire_days("1-5/2"), days(&[1, 3, 5]));
    assert_eq!(fire_days("1,3,5"), days(&[1, 3, 5]));
    assert_eq!(fire_days("*"), days(&[0, 1, 2, 3, 4, 5, 6]));
    assert_eq!(fire_days("?"), days(&[0, 1, 2, 3, 4, 5, 6]));
}

#[test]
fn a_range_ending_at_seven_wraps_to_sunday() {
    assert_eq!(fire_days("5-7"), days(&[5, 6, 0]));
    assert_eq!(fire_days("0-7"), days(&[0, 1, 2, 3, 4, 5, 6]));
    assert_eq!(fire_days("6-7"), days(&[6, 0]));
}

#[test]
fn names_pass_through_and_mix_with_numbers() {
    assert_eq!(
        normalize_expression("0 9 * * MON-FRI").unwrap(),
        "0 0 9 * * MON-FRI"
    );
    assert_eq!(fire_days("MON-FRI"), days(&[1, 2, 3, 4, 5]));
    assert_eq!(fire_days("fri"), days(&[5]));
    assert_eq!(fire_days("MON,3,fri"), days(&[1, 3, 5]));
    assert_eq!(fire_days("SUN,6"), days(&[0, 6]));
}

#[test]
fn weekday_values_outside_zero_to_seven_are_rejected() {
    for dow in ["8", "0-8", "9,1", "1-10/2"] {
        let expr = format!("0 9 * * {dow}");
        let err = normalize_expression(&expr).unwrap_err();
        assert!(
            err.to_string().contains("day of week"),
            "{expr}: unexpected error {err:#}"
        );
        assert!(validate_schedule(&utc_cron(&expr), monday()).is_err());
    }
}

#[test]
fn six_and_seven_field_expressions_keep_crate_native_weekdays() {
    // Crate-native numbering is 1 = Sunday, so `5` here is Thursday.
    assert_eq!(normalize_expression("0 0 9 * * 5").unwrap(), "0 0 9 * * 5");
    assert_eq!(
        normalize_expression("0 0 9 * * 1-5 2027").unwrap(),
        "0 0 9 * * 1-5 2027"
    );
    let next = next_run_for_schedule(&utc_cron("0 0 9 * * 5"), monday()).unwrap();
    assert_eq!(next.weekday(), chrono::Weekday::Thu);
}

/// Every numeric POSIX range and step over 0–7 fires on exactly the days
/// POSIX cron would, with 7 folded onto Sunday.
#[test]
fn every_numeric_range_and_step_matches_posix() {
    for start in 0..=7u32 {
        for end in start..=7 {
            for step in 1..=7u32 {
                let expected: BTreeSet<u32> = (start..=end)
                    .step_by(step as usize)
                    .map(|d| d % 7)
                    .collect();
                let field = format!("{start}-{end}/{step}");
                assert_eq!(fire_days(&field), expected, "{field}");
            }
            let field = format!("{start}-{end}");
            let expected: BTreeSet<u32> = (start..=end).map(|d| d % 7).collect();
            assert_eq!(fire_days(&field), expected, "{field}");
        }
        let expected: BTreeSet<u32> = (start..=7).step_by(2).map(|d| d % 7).collect();
        assert_eq!(fire_days(&format!("{start}/2")), expected, "{start}/2");
    }
}
