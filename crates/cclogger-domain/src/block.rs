//! Blocks: continuous stretches of work on the global cross-session timeline.
//!
//! [`blocks`] clusters instants with [`crate::clock::cluster`] -- the same basis
//! `cclog report`'s agent clock uses, for the same measured reasons (see that
//! module and issue #22) -- and then describes what each resulting span is made of:
//! which repositories were active inside it, and how many observations and prompts
//! each contributed.
//!
//! **A block that spans two repositories stays one block.** Concurrent work is one
//! stretch of wall-clock time; splitting it per repository would report the same
//! minutes twice, which is the trap this project keeps fighting (design §13). The
//! corollary: an observation that carries no repository is never dropped -- it
//! becomes its own [`BlockPart`], keyed by `None`, so every observation inside a
//! block's span is accounted for by exactly one part.

use crate::clock::{Span, cluster};
use std::collections::BTreeMap;

/// One stretch of continuous work on the global timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub span: Span,
    /// Repositories active inside it, each with its own observation count and
    /// prompt count, ordered by observation count descending, then named
    /// repositories by name, with the no-repository part last: it is the block's
    /// residual, and leading a rendered list with it reads as though it were what
    /// the block was mostly about.
    pub parts: Vec<BlockPart>,
}

/// One repository's share of a [`Block`] -- or, when `repository` is `None`, the
/// share belonging to observations that carried no repository at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockPart {
    /// `None` = the observation carried no repository. Kept as its own part, never
    /// folded into a named repository's count or silently dropped.
    pub repository: Option<String>,
    pub observations: u64,
    pub prompts: u64,
}

/// Cluster `events` -- `(instant, repository, is_prompt)`, in any order -- into
/// [`Block`]s using [`cluster`]'s gap threshold, and describe each block's
/// per-repository composition.
///
/// Reuses `cluster` rather than re-deriving spans: its threshold semantics (a gap
/// exactly at `gap` does not split) are pinned by its own tests and must not diverge
/// from what `cclog report`'s agent clock does with the same events.
pub fn blocks(events: &[(i64, Option<String>, bool)], gap: i64) -> Vec<Block> {
    let instants: Vec<i64> = events.iter().map(|(t, _, _)| *t).collect();
    let spans = cluster(&instants, gap);

    // `spans` is built from exactly the instants in `events`, so every event's
    // instant falls inside exactly one span. Spans are disjoint and `end` strictly
    // increases across them (a new cluster only starts once a gap exceeds the
    // threshold), so the first span whose `end` has not yet reached `t` is the one
    // that contains it.
    let mut counts: Vec<BTreeMap<Option<String>, (u64, u64)>> = vec![BTreeMap::new(); spans.len()];
    for (t, repository, is_prompt) in events {
        let index = spans.partition_point(|span| span.end < *t);
        let entry = counts[index].entry(repository.clone()).or_insert((0, 0));
        entry.0 += 1;
        if *is_prompt {
            entry.1 += 1;
        }
    }

    spans
        .into_iter()
        .zip(counts)
        .map(|(span, counts)| Block {
            span,
            parts: parts_from_counts(counts),
        })
        .collect()
}

/// Turn per-repository `(observations, prompts)` counts into [`BlockPart`]s ordered
/// by observation count descending, then named repositories alphabetically, and the
/// `None` part last.
///
/// `None` last is deliberate, not [`Option`]'s derived order (which puts it first).
/// A part with no repository is the block's residual -- the observations it could not
/// attribute -- and a renderer walking these in order leads with it otherwise, so a
/// block tied on observation count would print "no repository" ahead of a repository
/// it does know. Leading with the least informative part reads as though it were the
/// block's main subject. The report already sorts `unassigned` last for the same
/// reason; this keeps the two views consistent.
fn parts_from_counts(counts: BTreeMap<Option<String>, (u64, u64)>) -> Vec<BlockPart> {
    let mut parts: Vec<BlockPart> = counts
        .into_iter()
        .map(|(repository, (observations, prompts))| BlockPart {
            repository,
            observations,
            prompts,
        })
        .collect();
    parts.sort_by(|a, b| {
        b.observations
            .cmp(&a.observations)
            .then_with(|| a.repository.is_none().cmp(&b.repository.is_none()))
            .then_with(|| a.repository.cmp(&b.repository))
    });
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: i64, repo: &str, p: bool) -> (i64, Option<String>, bool) {
        (t, Some(repo.to_string()), p)
    }

    #[test]
    fn instants_closer_than_the_gap_form_one_block() {
        let b = blocks(&[ev(0, "a", true), ev(100, "a", false)], 300);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].span, Span { start: 0, end: 100 });
    }

    #[test]
    fn a_gap_larger_than_the_threshold_starts_a_new_block() {
        let b = blocks(&[ev(0, "a", true), ev(1000, "a", true)], 300);
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn two_repositories_active_together_stay_in_one_block_rather_than_splitting_it() {
        // The overlap is the structure worth seeing: a block that hid it by
        // splitting per repository would report the same minutes twice.
        let b = blocks(
            &[ev(0, "a", true), ev(50, "b", true), ev(100, "a", false)],
            300,
        );
        assert_eq!(b.len(), 1, "concurrent work is one stretch of time");
        assert_eq!(b[0].parts.len(), 2);
    }

    #[test]
    fn a_blocks_parts_are_ordered_by_weight_then_name_so_the_render_is_stable() {
        let b = blocks(
            &[
                ev(0, "z", false),
                ev(1, "z", false),
                ev(2, "a", false),
                ev(3, "m", false),
            ],
            300,
        );
        let names: Vec<&str> = b[0]
            .parts
            .iter()
            .map(|p| p.repository.as_deref().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["z", "a", "m"],
            "two observations beat one, then alphabetical"
        );
    }

    #[test]
    fn block_order_does_not_depend_on_input_order() {
        let a = blocks(&[ev(1000, "a", true), ev(0, "b", true)], 300);
        let b = blocks(&[ev(0, "b", true), ev(1000, "a", true)], 300);
        assert_eq!(a.len(), b.len());
        assert_eq!(a[0].span, b[0].span);
        assert_eq!(a[1].span, b[1].span);
    }

    #[test]
    fn prompts_are_counted_separately_from_observations() {
        let b = blocks(
            &[ev(0, "a", true), ev(10, "a", false), ev(20, "a", true)],
            300,
        );
        assert_eq!(b[0].parts[0].observations, 3);
        assert_eq!(b[0].parts[0].prompts, 2);
    }

    #[test]
    fn an_observation_with_no_repository_is_kept_as_its_own_part_not_dropped() {
        // Dropping it would make the block's parts fail to account for its own
        // observations, which is the accounting hole the importer exists to close.
        let b = blocks(&[ev(0, "a", false), (10, None, false)], 300);
        assert_eq!(b[0].parts.len(), 2);
        assert_eq!(
            b[0].parts.iter().map(|p| p.observations).sum::<u64>(),
            2,
            "every observation in the span is accounted for by exactly one part"
        );
    }

    #[test]
    fn the_part_with_no_repository_sorts_after_every_named_one_at_the_same_weight() {
        // `Option`'s derived order would put it first, which reads -- once rendered as
        // a list under a block -- as though "no repository" were what the block was
        // mostly about. It is the residual, so it goes last. Both repositories carry
        // one observation here, so nothing but this tie-break decides the order.
        let b = blocks(
            &[(0, None, false), ev(10, "z", false), ev(20, "a", false)],
            300,
        );
        let order: Vec<Option<&str>> = b[0].parts.iter().map(|p| p.repository.as_deref()).collect();
        assert_eq!(
            order,
            vec![Some("a"), Some("z"), None],
            "named repositories first, alphabetically; the residual last"
        );

        // Weight still outranks the tie-break: a heavier residual is not demoted to
        // last, or the ordering would be by identity rather than by how much of the
        // block each part accounts for.
        let heavy = blocks(
            &[(0, None, false), (5, None, false), ev(10, "a", false)],
            300,
        );
        assert_eq!(
            heavy[0]
                .parts
                .iter()
                .map(|p| p.repository.as_deref())
                .collect::<Vec<_>>(),
            vec![None, Some("a")],
            "two observations still beat one, whichever part carries them"
        );
    }

    #[test]
    fn no_events_produce_no_blocks() {
        assert_eq!(blocks(&[], 300).len(), 0);
    }

    #[test]
    fn a_single_event_is_a_zero_length_block() {
        let b = blocks(&[ev(42, "a", true)], 300);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].span, Span { start: 42, end: 42 });
    }
}
