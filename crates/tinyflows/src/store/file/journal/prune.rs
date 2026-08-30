//! Prunes old notes without breaking supersession chains.

use crate::store::types::WorkflowNote;

use super::MAX_NOTES;

/// Drop the oldest notes past [`MAX_NOTES`].
///
/// Pinned notes are protected. Supersession chains are pruned as whole groups:
/// a recent replacement and its predecessor survive together, while an old
/// chain can eventually leave together without a dangling `superseded_by`.
pub(super) fn prune(notes: &mut Vec<WorkflowNote>) {
    if notes.len() <= MAX_NOTES {
        return;
    }

    let by_id: std::collections::HashMap<&str, usize> = notes
        .iter()
        .enumerate()
        .map(|(index, note)| (note.id.as_str(), index))
        .collect();
    let mut parents: Vec<usize> = (0..notes.len()).collect();
    for (index, note) in notes.iter().enumerate() {
        if let Some(replacement) = note.superseded_by.as_deref().and_then(|id| by_id.get(id)) {
            join_groups(&mut parents, index, *replacement);
        }
    }

    let mut groups: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
    for index in 0..notes.len() {
        let root = group_root(&mut parents, index);
        groups.entry(root).or_default().push(index);
    }
    let mut droppable: Vec<Vec<usize>> = groups
        .into_values()
        .filter(|group| group.iter().all(|index| !notes[*index].pinned))
        .collect();
    droppable.sort_by(|a, b| {
        let oldest = |group: &[usize]| {
            group
                .iter()
                .map(|index| notes[*index].id.as_str())
                .min()
                .unwrap_or_default()
        };
        oldest(a).cmp(oldest(b))
    });

    let needed = notes.len() - MAX_NOTES;
    let mut removed = 0;
    let mut doomed = std::collections::HashSet::new();
    for group in droppable {
        if removed >= needed {
            break;
        }
        removed += group.len();
        doomed.extend(group);
    }
    let mut index = 0;
    notes.retain(|_| {
        let keep = !doomed.contains(&index);
        index += 1;
        keep
    });
}

/// Find a supersession group's root while compressing the traversed path.
fn group_root(parents: &mut [usize], index: usize) -> usize {
    if parents[index] != index {
        let parent = parents[index];
        parents[index] = group_root(parents, parent);
    }
    parents[index]
}

/// Join two notes into one indivisible supersession group.
fn join_groups(parents: &mut [usize], left: usize, right: usize) {
    let left = group_root(parents, left);
    let right = group_root(parents, right);
    if left != right {
        parents[right] = left;
    }
}
