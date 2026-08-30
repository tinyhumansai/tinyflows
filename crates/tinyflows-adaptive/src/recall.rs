//! What a planner is told about the past.
//!
//! Two different pasts, and conflating them is how a retry becomes a repeat.
//!
//! **This episode's attempts** are specific: three rows saying what was tried
//! and why each fell short. They are the reason attempt four is not attempt
//! two in different words. Without them the author writes the same graph again,
//! confidently, because nothing told it otherwise.
//!
//! **Lessons** are general: what generalised out of *other* episodes. They come
//! from [`crate::closing::consolidate`], which until now was write-only —
//! lessons were being kept and never read, which is a knowledge store that
//! costs money and returns nothing.
//!
//! Both are rendered for a prompt here rather than at the two call sites, so
//! `select` and `author` see the same history in the same words.

use crate::ledger::{LedgerRow, Lesson, LessonKind};

/// How many lessons a planner sees, beyond the kinds that always load.
///
/// **Everything, and that is the right answer at this scale.** With tens of
/// lessons in scope, every one of them is relevant to the planner reading them,
/// and the ordering below is a placeholder for matching nobody has written yet.
/// Capping on an unvalidated order does not select the best five, it discards
/// four-fifths of what was learned on a guess.
///
/// It was five, and that was a bug rather than a trade: a lesson written
/// moments ago has `applied == 0`, so its help rate is `0.0`, so it sorted
/// level with lessons proven useless and was cut the moment five others had any
/// success. Never shown, so never applied, so never able to earn a rate — the
/// trap [`crate::promotion`] avoids by giving a variant its trials, with
/// nothing here doing the same.
///
/// The seam stays because a host with hundreds of lessons has a real prompt-size
/// problem: pass your own `k` to [`retrieve`], and the ordering below decides
/// what survives.
pub const RECALL_LIMIT: usize = usize::MAX;

/// Kinds that load wholesale, exempt from [`RECALL_LIMIT`].
///
/// A constraint is a limit no approach can cross. Inside its scope it is always
/// relevant, there are few of them, and dropping one because five strategies
/// outranked it means proposing something already known to be impossible.
const LOAD_ALL: [LessonKind; 1] = [LessonKind::Constraint];

/// Where a lesson sorts, when only some of them can be shown.
///
/// Three bands rather than one number, because a rate cannot tell "has not been
/// tried" from "has been tried and never helped" — both are `0.0`, and
/// collapsing them means a cap silently prefers a known failure to an untested
/// idea.
fn band(lesson: &Lesson) -> u8 {
    match (lesson.applied, lesson.helped) {
        // Demonstrably useful at least once.
        (a, h) if a > 0 && h > 0 => 0,
        // Never put in front of a planner. Unjudged, not bad.
        (0, _) => 1,
        // Applied, and never once helped.
        _ => 2,
    }
}

/// Choose which lessons a planner sees.
///
/// Everything in scope by default — see [`RECALL_LIMIT`]. The order matters
/// only when a host passes a smaller `k`, and then it is by band first (useful,
/// untried, useless), rate within the first band, and id to break ties so a
/// planner does not see a different set each attempt.
#[must_use]
pub fn retrieve(lessons: Vec<Lesson>, kind: Option<LessonKind>, k: usize) -> Vec<Lesson> {
    let mut pool: Vec<Lesson> = lessons
        .into_iter()
        .filter(|lesson| kind.is_none_or(|want| lesson.kind == want))
        .collect();
    pool.sort_by(|a, b| {
        band(a)
            .cmp(&band(b))
            .then_with(|| b.help_rate().total_cmp(&a.help_rate()))
            .then_with(|| a.id.cmp(&b.id))
    });

    let (always, rest): (Vec<Lesson>, Vec<Lesson>) =
        pool.into_iter().partition(|l| LOAD_ALL.contains(&l.kind));
    always.into_iter().chain(rest.into_iter().take(k)).collect()
}

/// What generalised out of other episodes, for a prompt. Empty when nothing has.
#[must_use]
pub fn render_lessons(lessons: &[Lesson]) -> String {
    if lessons.is_empty() {
        return String::new();
    }
    let body = lessons
        .iter()
        .map(|lesson| {
            let mechanism = if lesson.mechanism.is_empty() {
                String::new()
            } else {
                format!(" ({})", lesson.mechanism)
            };
            let record = match lesson.applied {
                0 => "not yet applied".to_string(),
                applied => format!("applied {applied}×, helped {}×", lesson.helped),
            };
            format!(
                "- when {}: {}{mechanism} [{record}]",
                lesson.trigger, lesson.claim
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("\n\n# Learned from earlier episodes\n{body}")
}

/// What this episode has already spent, for a prompt. Empty on attempt one.
///
/// Numbered from one, the way a person counts attempts, and each line carries
/// the signature — the planner is being asked not to propose one of these
/// again, so it needs to see them the way the exclusion list does.
#[must_use]
pub fn render_history(rows: &[LedgerRow]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let body = rows
        .iter()
        .map(|row| {
            let because = if row.cause.is_empty() {
                String::new()
            } else {
                format!("\n  still missing: {}", row.cause)
            };
            format!(
                "{}. [{}] {} → {}{because}",
                row.attempt, row.approach_sig, row.approach_desc, row.outcome
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("\n\n# Already tried this episode — do not propose any of these again\n{body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lesson(id: &str, kind: LessonKind, applied: u32, helped: u32) -> Lesson {
        Lesson {
            id: id.into(),
            kind,
            trigger: format!("the {id} situation"),
            mechanism: String::new(),
            claim: format!("do {id}"),
            applied,
            helped,
            scope_key: None,
        }
    }

    fn row(attempt: u32, sig: &str, cause: &str) -> LedgerRow {
        LedgerRow {
            id: format!("r{attempt}"),
            episode: "ep".into(),
            attempt,
            approach_sig: sig.into(),
            approach_desc: "tried the obvious thing".into(),
            workflow_id: None,
            outcome: "fell short".into(),
            cause: cause.into(),
            cost_usd: 0.0,
            at: "2026-01-01T00:00:00Z".into(),
            satisfied: false,
            advanced: false,
        }
    }

    #[test]
    fn everything_in_scope_reaches_the_planner_by_default() {
        // The default is not a selection. With tens of lessons every one is
        // relevant, and cutting on an unvalidated order discards what was
        // learned on a guess.
        let pool: Vec<Lesson> = (0..20)
            .map(|n| lesson(&format!("l{n}"), LessonKind::Strategy, 10, 10))
            .collect();
        assert_eq!(retrieve(pool, None, RECALL_LIMIT).len(), 20);
    }

    #[test]
    fn a_brand_new_lesson_is_not_cut_before_a_useless_one() {
        // The bug the default hid. A lesson written moments ago has no rate, so
        // it sorted level with lessons proven useless and was dropped first —
        // never shown, so never applied, so never able to earn a rate.
        let pool = vec![
            lesson("useless", LessonKind::Strategy, 9, 0),
            lesson("brand-new", LessonKind::Strategy, 0, 0),
        ];
        let got = retrieve(pool, None, 1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "brand-new", "untried outranks proven useless");
    }

    #[test]
    fn a_lesson_that_has_helped_still_outranks_an_untried_one() {
        let pool = vec![
            lesson("untried", LessonKind::Strategy, 0, 0),
            lesson("works", LessonKind::Strategy, 4, 3),
        ];
        let got = retrieve(pool, None, 1);
        assert_eq!(got[0].id, "works");
    }

    #[test]
    fn the_best_helping_lessons_come_first() {
        let got = retrieve(
            vec![
                lesson("weak", LessonKind::Strategy, 10, 1),
                lesson("strong", LessonKind::Strategy, 10, 9),
            ],
            None,
            5,
        );
        assert_eq!(got[0].id, "strong");
    }

    #[test]
    fn the_order_is_stable_when_two_lessons_are_equally_good() {
        // A planner shown a different five each attempt cannot be reasoned about.
        let pool = vec![
            lesson("b", LessonKind::Strategy, 4, 2),
            lesson("a", LessonKind::Strategy, 4, 2),
        ];
        let once = retrieve(pool.clone(), None, 5);
        let twice = retrieve(pool, None, 5);
        assert_eq!(once[0].id, "a");
        assert_eq!(
            once.iter().map(|l| &l.id).collect::<Vec<_>>(),
            twice.iter().map(|l| &l.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn constraints_load_wholesale_past_the_cap() {
        // Dropping a constraint because five strategies outranked it means
        // proposing something already known to be impossible.
        let mut pool: Vec<Lesson> = (0..8)
            .map(|n| lesson(&format!("s{n}"), LessonKind::Strategy, 10, 10))
            .collect();
        pool.push(lesson("hard-limit", LessonKind::Constraint, 0, 0));

        let got = retrieve(pool, None, 2);
        assert!(got.iter().any(|l| l.id == "hard-limit"), "{got:?}");
        assert_eq!(got.len(), 3, "the constraint plus the two-strategy cap");
    }

    #[test]
    fn nothing_learned_yet_renders_to_nothing_rather_than_an_empty_heading() {
        assert!(render_lessons(&[]).is_empty());
        assert!(render_history(&[]).is_empty());
    }

    #[test]
    fn the_history_names_the_signature_the_exclusion_list_uses() {
        let rendered = render_history(&[row(1, "selected:weekly", "no numbers in it")]);
        assert!(rendered.contains("[selected:weekly]"), "{rendered}");
        assert!(rendered.contains("do not propose any of these again"));
        assert!(rendered.contains("still missing: no numbers in it"));
    }

    #[test]
    fn a_row_with_no_stated_cause_says_nothing_rather_than_an_empty_line() {
        let rendered = render_history(&[row(2, "authored:abc", "")]);
        assert!(!rendered.contains("still missing"), "{rendered}");
    }

    #[test]
    fn an_unapplied_lesson_says_so_rather_than_showing_zero_of_zero() {
        let rendered = render_lessons(&[lesson("new", LessonKind::Strategy, 0, 0)]);
        assert!(rendered.contains("not yet applied"), "{rendered}");
    }
}
