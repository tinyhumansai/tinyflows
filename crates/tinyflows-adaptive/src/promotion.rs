//! Which member of a repaired family the catalogue offers.
//!
//! [`crate::closing::repair`] never edits a workflow in place — it saves a
//! variant, so the parent's score survives to be compared against. That leaves
//! a question this module answers: after three repairs, which of the four
//! graphs does a planner get to see?
//!
//! Showing all four is the wrong answer. They are near-identical, their
//! descriptions differ by a clause, and a planner choosing between them is
//! choosing noise. Showing the newest is also wrong — that is promotion by
//! having been written, which is what the whole variant mechanism exists to
//! avoid.
//!
//! So the catalogue offers **one member per family**, and this decides which.
//!
//! # The rule
//!
//! Three bands, in order, and the first non-empty one wins:
//!
//! 1. **Proven and has helped** — [`MIN_TRIALS`] runs behind it and at least one
//!    success. Best help rate, ties broken by more trials: 40/40 beats 1/1 at
//!    the same rate, because they are not the same evidence.
//! 2. **Unproven** — too few runs to say. Ordered by what thin evidence there
//!    is, then by lineage, so a family where *nothing* has been tried keeps the
//!    graph a person wrote.
//! 3. **Proven and never helped** — enough runs to be sure it does not work.
//!
//! The bands exist because "not yet tried" and "tried and never worked" are
//! both a help rate of `0.0`, and a single number cannot tell them apart. With
//! one number the filter ran first, so if the *only* proven member had never
//! helped it won by default — a root that failed three times out of three
//! holding the slot against a variant that had succeeded twice out of two. The
//! same shape as the bug in [`crate::recall`], arrived at independently.
//!
//! # Why there is no exploration policy
//!
//! A fresh variant has zero trials, so it can never become proven if it is
//! never offered — the usual explore/exploit trap, and the usual fix is to
//! offer unproven candidates some fraction of the time.
//!
//! That machinery is not needed here, because of where variants come from. A
//! variant is written by the closing pass of an episode whose *parent just
//! failed*, and that parent is already in the episode's exclusion list. The
//! next attempt of that same episode cannot pick the parent, so the variant
//! gets its trials exactly where the evidence is most relevant — against the
//! goal that broke the parent — without anyone writing a bandit.
//!
//! The cost of getting this wrong in the other direction is what the rule
//! protects: an unproven variant that displaced a 40/40 parent for everyone
//! would spend other people's episodes discovering it was worse.

use crate::ledger::Score;

/// Runs before a member's score is treated as evidence.
///
/// Three, not one: a single satisfied run is 1/1, indistinguishable by rate
/// from forty, and promoting on it means promoting on luck. Three is small
/// enough that a genuinely better variant takes over quickly and large enough
/// that a coin flip usually does not.
pub const MIN_TRIALS: u32 = 3;

/// Where one member of a family stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// Not enough runs to say. Gets its trials from the episode that made it.
    Unproven,
    /// Proven, and the best of the family. This is what the catalogue offers.
    Champion,
    /// Proven, and something else in the family is better.
    Beaten,
}

/// Which band a member sits in. Lower is better; see the module note.
fn band(score: Score) -> u8 {
    match (score.applied >= MIN_TRIALS, score.helped > 0) {
        (true, true) => 0,
        (false, _) => 1,
        (true, false) => 2,
    }
}

/// Pick the member to offer.
///
/// `family` is `(id, score)` in [`crate::ledger::Ledger::lineage`] order —
/// **root first**, which is what decides an unproven family: nothing has
/// established anything, so the graph a person wrote keeps the position.
/// Returns `None` only for an empty family.
#[must_use]
pub fn champion(family: &[(String, Score)]) -> Option<&str> {
    family
        .iter()
        .enumerate()
        .min_by(|(i, (_, a)), (j, (_, b))| {
            band(*a)
                .cmp(&band(*b))
                .then_with(|| b.help_rate().total_cmp(&a.help_rate()))
                .then_with(|| b.applied.cmp(&a.applied))
                // Lineage order last, so a tie inside a band keeps the root.
                .then_with(|| i.cmp(j))
        })
        .map(|(_, (id, _))| id.as_str())
}

/// Where `id` stands within its family.
#[must_use]
pub fn standing(id: &str, family: &[(String, Score)]) -> Standing {
    let Some((_, score)) = family.iter().find(|(member, _)| member == id) else {
        return Standing::Unproven;
    };
    if band(*score) == 1 {
        return Standing::Unproven;
    }
    if champion(family) == Some(id) {
        Standing::Champion
    } else {
        Standing::Beaten
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn family(members: &[(&str, u32, u32)]) -> Vec<(String, Score)> {
        members
            .iter()
            .map(|(id, applied, helped)| {
                (
                    (*id).to_string(),
                    Score {
                        applied: *applied,
                        helped: *helped,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn a_lone_workflow_is_its_own_champion() {
        assert_eq!(champion(&family(&[("weekly", 0, 0)])), Some("weekly"));
    }

    #[test]
    fn a_fresh_variant_does_not_displace_a_proven_parent() {
        // The expensive mistake: an untested graph taking over for everyone and
        // spending other people's episodes finding out it was worse.
        let f = family(&[("weekly", 40, 40), ("weekly-fix-abc", 0, 0)]);
        assert_eq!(champion(&f), Some("weekly"));
        assert_eq!(standing("weekly-fix-abc", &f), Standing::Unproven);
    }

    #[test]
    fn a_variant_takes_over_once_it_has_proven_better() {
        let f = family(&[("weekly", 10, 5), ("weekly-fix-abc", 4, 4)]);
        assert_eq!(champion(&f), Some("weekly-fix-abc"));
        assert_eq!(standing("weekly", &f), Standing::Beaten);
        assert_eq!(standing("weekly-fix-abc", &f), Standing::Champion);
    }

    #[test]
    fn a_variant_proven_worse_stays_out() {
        let f = family(&[("weekly", 10, 9), ("weekly-fix-abc", 5, 1)]);
        assert_eq!(champion(&f), Some("weekly"));
        assert_eq!(standing("weekly-fix-abc", &f), Standing::Beaten);
    }

    #[test]
    fn more_trials_win_the_tie_because_they_are_not_the_same_evidence() {
        // 40/40 and 3/3 are the same rate. They are not the same claim.
        let f = family(&[("weekly", 40, 40), ("weekly-fix-abc", 3, 3)]);
        assert_eq!(champion(&f), Some("weekly"));
    }

    #[test]
    fn an_untried_family_keeps_the_graph_a_person_wrote() {
        // The principle the lineage tie-break exists for: with no evidence at
        // all, nothing displaces the root.
        let f = family(&[("weekly", 0, 0), ("weekly-fix-abc", 0, 0)]);
        assert_eq!(champion(&f), Some("weekly"));
    }

    #[test]
    fn a_fresh_variant_does_not_displace_an_only_slightly_tried_root() {
        let f = family(&[("weekly", 1, 1), ("weekly-fix-abc", 0, 0)]);
        assert_eq!(champion(&f), Some("weekly"));
    }

    #[test]
    fn thin_evidence_still_decides_between_two_unproven_members() {
        // This case used to assert the root wins, on the reading that neither
        // is proven so neither takes it. But the root here has been tried once
        // and failed, and the variant twice and worked twice — "unproven" is
        // not "untested", and offering the one that has only ever failed wastes
        // the attempt that would have told us either way.
        let f = family(&[("weekly", 1, 0), ("weekly-fix-abc", 2, 2)]);
        assert_eq!(champion(&f), Some("weekly-fix-abc"));
    }

    #[test]
    fn one_proven_member_wins_even_when_the_root_is_unproven() {
        let f = family(&[("weekly", 2, 0), ("weekly-fix-abc", 3, 2)]);
        assert_eq!(champion(&f), Some("weekly-fix-abc"));
    }

    #[test]
    fn a_workflow_outside_the_family_reads_as_unproven_rather_than_panicking() {
        let f = family(&[("weekly", 40, 40)]);
        assert_eq!(standing("something-else", &f), Standing::Unproven);
    }

    #[test]
    fn a_workflow_proven_useless_does_not_outrank_an_untested_variant() {
        // The mirror of the recall bug. There, an untried lesson sorted level
        // with useless ones and was cut. Here, the proven filter runs first, so
        // if the ONLY proven member has never helped it wins by default — a
        // root that failed three times out of three keeping the slot against a
        // variant that has succeeded twice out of two.
        let f = family(&[("weekly", 3, 0), ("weekly-fix-1", 2, 2)]);
        assert_eq!(champion(&f), Some("weekly-fix-1"));
        assert_eq!(standing("weekly", &f), Standing::Beaten);
    }

    #[test]
    fn one_success_still_beats_an_untested_variant() {
        // The other direction: a member that has actually worked keeps the slot
        // against something with no record, which is the whole point of the
        // trial threshold.
        let f = family(&[("weekly", 4, 1), ("weekly-fix-1", 2, 2)]);
        assert_eq!(champion(&f), Some("weekly"));
    }

    #[test]
    fn an_empty_family_has_no_champion() {
        assert_eq!(champion(&[]), None);
    }
}
