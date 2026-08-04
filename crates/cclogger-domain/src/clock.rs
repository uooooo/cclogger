//! Pure interval arithmetic over epoch seconds: no I/O, no clock, no RNG.
//!
//! Four functions build on one shape, [`Span`], a half-open `[start, end)` interval:
//! [`union_seconds`] and [`merge`] collapse overlap, [`cluster`] groups instants
//! separated by small gaps, and [`partition_by_nearest_anchor`] is the one the
//! project's design leans on -- it cuts a union of windows into atomic intervals and
//! gives each to exactly one label, so per-label shares always sum back to the union
//! they came from. Overlapping attention is real (design §13); this is what keeps a
//! report from double-counting it while still showing it.

use std::collections::BTreeMap;

/// A half-open interval [start, end) in whole seconds since the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Span {
    pub start: i64,
    pub end: i64,
}

/// Total covered time, overlaps counted once.
pub fn union_seconds(spans: &[Span]) -> i64 {
    merge(spans).iter().map(|span| span.end - span.start).sum()
}

/// Merge overlapping/adjacent spans into a minimal ordered set.
pub fn merge(spans: &[Span]) -> Vec<Span> {
    let mut sorted = spans.to_vec();
    sorted.sort();

    let mut merged: Vec<Span> = Vec::with_capacity(sorted.len());
    for span in sorted {
        match merged.last_mut() {
            // Touching (span.start == last.end) or overlapping: extend in place.
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

/// Group instants into spans, starting a new one when the gap exceeds `gap`.
pub fn cluster(instants: &[i64], gap: i64) -> Vec<Span> {
    let mut sorted = instants.to_vec();
    sorted.sort();

    let mut clusters: Vec<Span> = Vec::new();
    for instant in sorted {
        match clusters.last_mut() {
            // The threshold itself is still one cluster -- only a gap that exceeds
            // `gap` starts a new one.
            Some(last) if instant - last.end <= gap => last.end = instant,
            _ => clusters.push(Span {
                start: instant,
                end: instant,
            }),
        }
    }
    clusters
}

/// Split the union of `anchors`' windows into atomic intervals and give each
/// to its nearest anchor's label. Shares sum to the union.
///
/// Each anchor `(t, label)` claims the window `[t - before, t + after)`. The union of
/// all windows is cut into atomic intervals at every window edge, so within one
/// atomic interval no window can start or end -- each window either covers it
/// entirely or not at all. An interval covered by no window (a gap between windows)
/// contributes nothing. An interval covered by one or more windows goes entirely to
/// the covering anchor nearest its midpoint, so every second of the union is
/// allocated exactly once; ties go to the lexicographically smaller label, which
/// makes the result independent of `anchors`' input order.
pub fn partition_by_nearest_anchor<L: Clone + Ord>(
    anchors: &[(i64, L)],
    before: i64,
    after: i64,
) -> Vec<(L, i64)> {
    let windows: Vec<Span> = anchors
        .iter()
        .map(|(t, _)| Span {
            start: t - before,
            end: t + after,
        })
        .collect();

    let mut edges: Vec<i64> = windows.iter().flat_map(|w| [w.start, w.end]).collect();
    edges.sort();
    edges.dedup();

    let mut totals: BTreeMap<L, i64> = BTreeMap::new();
    for pair in edges.windows(2) {
        let (lo, hi) = (pair[0], pair[1]);
        // Twice the midpoint, so nearness compares as integers with no rounding.
        let mid2 = lo + hi;

        let mut nearest: Option<(i64, &L)> = None;
        for ((t, label), w) in anchors.iter().zip(&windows) {
            if w.start > lo || hi > w.end {
                continue; // this window does not cover the atomic interval
            }
            let distance = (mid2 - 2 * *t).abs();
            let replace = match nearest {
                None => true,
                Some((best_distance, best_label)) => {
                    distance < best_distance || (distance == best_distance && label < best_label)
                }
            };
            if replace {
                nearest = Some((distance, label));
            }
        }

        if let Some((_, label)) = nearest {
            *totals.entry(label.clone()).or_insert(0) += hi - lo;
        }
    }

    totals.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(a: i64, b: i64) -> Span {
        Span { start: a, end: b }
    }

    #[test]
    fn overlapping_spans_are_counted_once() {
        assert_eq!(union_seconds(&[s(0, 100), s(50, 150)]), 150);
    }

    #[test]
    fn disjoint_spans_add_up() {
        assert_eq!(union_seconds(&[s(0, 100), s(200, 300)]), 200);
    }

    #[test]
    fn a_span_wholly_inside_another_adds_nothing() {
        assert_eq!(union_seconds(&[s(0, 100), s(20, 30)]), 100);
    }

    #[test]
    fn touching_spans_merge_into_one() {
        assert_eq!(merge(&[s(0, 100), s(100, 200)]), vec![s(0, 200)]);
    }

    #[test]
    fn merge_returns_spans_in_order_regardless_of_input_order() {
        assert_eq!(
            merge(&[s(200, 300), s(0, 100)]),
            vec![s(0, 100), s(200, 300)]
        );
    }

    #[test]
    fn an_empty_input_has_no_duration_and_no_spans() {
        assert_eq!(union_seconds(&[]), 0);
        assert_eq!(merge(&[]), vec![]);
        assert_eq!(cluster(&[], 300), vec![]);
    }

    #[test]
    fn instants_closer_than_the_gap_stay_one_cluster() {
        assert_eq!(cluster(&[0, 100, 200], 300), vec![s(0, 200)]);
    }

    #[test]
    fn a_gap_larger_than_the_threshold_starts_a_new_cluster() {
        assert_eq!(
            cluster(&[0, 100, 1000], 300),
            vec![s(0, 100), s(1000, 1000)]
        );
    }

    #[test]
    fn a_gap_exactly_at_the_threshold_does_not_split() {
        // The threshold is the largest gap still considered continuous. Stated so
        // that a later change to `>` vs `>=` is a test failure, not a silent shift.
        assert_eq!(cluster(&[0, 300], 300), vec![s(0, 300)]);
    }

    #[test]
    fn a_single_instant_is_a_zero_length_cluster() {
        assert_eq!(cluster(&[42], 300), vec![s(42, 42)]);
    }

    #[test]
    fn partition_shares_sum_to_the_union_they_came_from() {
        let anchors = [(0i64, "a"), (100, "b"), (10_000, "a")];
        let shares = partition_by_nearest_anchor(&anchors, 60, 300);
        let windows: Vec<Span> = anchors.iter().map(|(t, _)| s(t - 60, t + 300)).collect();
        assert_eq!(
            shares.iter().map(|(_, n)| n).sum::<i64>(),
            union_seconds(&windows),
            "an atomic interval carries exactly one allocation (design §13)"
        );
    }

    #[test]
    fn overlapping_windows_from_two_labels_are_split_not_double_counted() {
        // The trap the acceptance document calls out: summing per-label unions
        // exceeds the day's real total whenever two labels overlap in time.
        let shares = partition_by_nearest_anchor(&[(0i64, "a"), (60, "b")], 60, 300);
        let total: i64 = shares.iter().map(|(_, n)| n).sum();
        assert_eq!(total, union_seconds(&[s(-60, 300), s(0, 360)]));
        assert!(
            shares.iter().all(|(_, n)| *n > 0),
            "both labels get a share"
        );
    }

    #[test]
    fn an_interval_goes_to_the_nearer_anchor_not_the_earlier_one() {
        // `b` starts after `a`, but their windows overlap on [140, 300), and that
        // overlap's midpoint (220) is nearer to `b` (100) than to `a` (0). A
        // partition that resolved overlaps by which anchor is earliest -- instead
        // of which is nearest -- would hand that whole 160s overlap to `a` instead,
        // making `a` the larger share instead of the smaller one asserted below.
        // (Non-overlapping anchors can't tell this apart from "earliest wins": every
        // interval then has only one covering anchor, so any tie-break rule agrees.)
        let shares = partition_by_nearest_anchor(&[(0i64, "a"), (200, "b")], 60, 300);
        let a = shares.iter().find(|(l, _)| *l == "a").unwrap().1;
        let b = shares.iter().find(|(l, _)| *l == "b").unwrap().1;
        assert_eq!(
            a, 200,
            "a keeps only the part of its window b's window does not reach"
        );
        assert_eq!(
            b, 360,
            "b, nearer to the overlap despite starting later, keeps the rest"
        );
    }

    #[test]
    fn an_anchor_whose_window_does_not_reach_an_interval_cannot_claim_it() {
        // "Nearest" means nearest *among the anchors whose window covers this
        // interval*, never nearest overall -- the distinction that made this
        // implementation disagree with a hand-measured acceptance document, and the
        // investigation conclude the document was wrong.
        //
        // The atomic interval [280, 300) lies inside x's window `[-60, 300)` and no
        // other. y's anchor is far closer to its midpoint (290 is 70 from y, 290 from
        // x), but y's window `[300, 660)` begins after the interval ends and never
        // reaches it. Allocating by nearest overall hands those 20 seconds to y:
        // v=20, x=340, y=380 -- the same total, split wrongly, which is exactly the
        // table this project corrected.
        let shares = partition_by_nearest_anchor(&[(-20i64, "v"), (0, "x"), (360, "y")], 60, 300);
        assert_eq!(shares, vec![("v", 20), ("x", 360), ("y", 360)]);

        let windows: Vec<Span> = [-20i64, 0, 360]
            .iter()
            .map(|t| s(t - 60, t + 300))
            .collect();
        assert_eq!(
            shares.iter().map(|(_, n)| n).sum::<i64>(),
            union_seconds(&windows),
            "the wrong split has the right total, so no sum-to-union assertion can catch it"
        );
    }

    #[test]
    fn a_tie_goes_to_the_lexicographically_smaller_label_in_either_input_order() {
        // Two prompts in the same second, in different repositories: every atomic
        // interval is exactly as near to one anchor as to the other, so nothing but
        // the documented tie-break decides where the time goes. Same-second prompts
        // are real -- 145 collisions in the measured corpus -- and a report whose
        // numbers moved with the order rows came back in would not be reproducible.
        let a_first = partition_by_nearest_anchor(&[(0i64, "a"), (0, "b")], 60, 300);
        let b_first = partition_by_nearest_anchor(&[(0i64, "b"), (0, "a")], 60, 300);
        assert_eq!(
            a_first,
            vec![("a", 360)],
            "the whole shared window goes to `a`, and `b` gets no row at all"
        );
        assert_eq!(
            b_first, a_first,
            "and the answer does not depend on the order"
        );
    }

    #[test]
    fn partition_is_deterministic_across_input_orderings() {
        // A plain `fn`, not a closure: the two calls below pass temporaries with
        // unrelated lifetimes, which needs a fresh lifetime per call (what a `fn`
        // item's elision gives for free). A closure infers one concrete lifetime
        // shared across all call sites, which these two temporaries cannot both
        // satisfy, and rustc rejects it as a lifetime error before either half of
        // this test's logic runs.
        fn f<'a>(v: &'a [(i64, &'a str)]) -> Vec<(&'a str, i64)> {
            let mut r = partition_by_nearest_anchor(v, 60, 300);
            r.sort();
            r
        }
        assert_eq!(
            f(&[(0, "a"), (100, "b")]),
            f(&[(100, "b"), (0, "a")]),
            "a report must not change because rows came back in another order"
        );
    }
}
