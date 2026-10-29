//! Non-destructive edit list: the ranges of the recording that are kept.
//!
//! Raw tracks are never modified; trimming and cutting only change this list,
//! and export renders it. Ranges are sorted, non-overlapping and non-empty.

use serde::{Deserialize, Serialize};

use crate::Micros;

/// Half-open `[start, end)` in source time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range(pub Micros, pub Micros);

impl Range {
    pub fn len(&self) -> Micros {
        self.1.saturating_sub(self.0)
    }

    pub fn is_empty(&self) -> bool {
        self.1 <= self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditList {
    keep: Vec<Range>,
}

impl EditList {
    /// Keep everything in a recording of `duration`.
    pub fn full(duration: Micros) -> Self {
        let keep = if duration > 0 {
            vec![Range(0, duration)]
        } else {
            Vec::new()
        };
        Self { keep }
    }

    pub fn ranges(&self) -> &[Range] {
        &self.keep
    }

    /// Whether the ranges are sorted, non-overlapping, non-empty and end by
    /// `duration`, as the edit operations keep them. Lists loaded from disk
    /// are checked with this.
    pub fn is_valid(&self, duration: Micros) -> bool {
        let mut prev_end = 0;
        self.keep.iter().all(|r| {
            let ok = !r.is_empty() && r.0 >= prev_end && r.1 <= duration;
            prev_end = r.1;
            ok
        })
    }

    /// Length of the edited output.
    pub fn duration(&self) -> Micros {
        self.keep.iter().map(Range::len).sum()
    }

    /// Remove `[start, end)` (source time) from the output.
    pub fn cut(&mut self, start: Micros, end: Micros) {
        if end <= start {
            return;
        }
        let mut out = Vec::with_capacity(self.keep.len() + 1);
        for &r in &self.keep {
            if r.1 <= start || r.0 >= end {
                out.push(r);
                continue;
            }
            if r.0 < start {
                out.push(Range(r.0, start));
            }
            if r.1 > end {
                out.push(Range(end, r.1));
            }
        }
        self.keep = out;
    }

    /// Like [`cut`], and also cuts what is left kept on either side of it
    /// if that is shorter than `min`, so cuts close together join rather
    /// than leave a flash of video between them.
    ///
    /// [`cut`]: Self::cut
    pub fn cut_joining(&mut self, start: Micros, end: Micros, min: Micros) {
        if end <= start {
            return;
        }
        self.cut(start, end);
        self.keep
            .retain(|r| r.len() >= min || (r.1 != start && r.0 != end));
    }

    /// The stretches removed from a recording of `duration`, in order: the
    /// gaps between kept ranges, including any trimmed at either end.
    pub fn cuts(&self, duration: Micros) -> Vec<Range> {
        let mut cuts = Vec::new();
        let mut last = 0;
        for r in &self.keep {
            if r.0 > last {
                cuts.push(Range(last, r.0));
            }
            last = r.1;
        }
        if last < duration {
            cuts.push(Range(last, duration));
        }
        cuts
    }

    /// This edit with the start (or `end`) of cut `index` (see [`cuts`])
    /// moved to `t`. A cut moved onto its other edge goes away; one moved
    /// over a neighbour joins it. `None` if there's no such cut.
    ///
    /// [`cuts`]: Self::cuts
    pub fn with_cut_edge_moved(
        &self,
        duration: Micros,
        index: usize,
        end: bool,
        t: Micros,
    ) -> Option<Self> {
        let mut cuts = self.cuts(duration);
        let cut = cuts.get_mut(index)?;
        let t = t.min(duration);
        if end {
            cut.1 = t;
        } else {
            cut.0 = t;
        }
        let mut edit = Self::full(duration);
        for c in cuts {
            edit.cut(c.0, c.1);
        }
        Some(edit)
    }

    /// Keep only `[start, end)` (source time), dropping everything outside it.
    pub fn trim(&mut self, start: Micros, end: Micros) {
        self.cut(0, start);
        self.cut(end, Micros::MAX);
    }

    /// Map an output (timeline) time to source time.
    pub fn to_source(&self, output: Micros) -> Option<Micros> {
        let mut acc = 0;
        for r in &self.keep {
            if output < acc + r.len() {
                return Some(r.0 + (output - acc));
            }
            acc += r.len();
        }
        None
    }

    /// How much is kept before source time `source`: its output time, and
    /// inside a cut, that of the kept time after it.
    pub fn kept_before(&self, source: Micros) -> Micros {
        self.keep.iter().map(|r| source.clamp(r.0, r.1) - r.0).sum()
    }

    /// Map a source time to output time, or `None` if it was cut.
    pub fn to_output(&self, source: Micros) -> Option<Micros> {
        let mut acc = 0;
        for r in &self.keep {
            if source < r.0 {
                return None;
            }
            if source < r.1 {
                return Some(acc + (source - r.0));
            }
            acc += r.len();
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_edges_move_shrink_and_join() {
        let mut e = EditList::full(100);
        e.cut(20, 30);
        e.cut(50, 60);
        e.cut(90, 100);
        assert_eq!(e.cuts(100), [Range(20, 30), Range(50, 60), Range(90, 100)]);
        // Shrink the first cut from its end, grow the second from its start.
        let moved = e.with_cut_edge_moved(100, 0, true, 25).unwrap();
        assert_eq!(
            moved.cuts(100),
            [Range(20, 25), Range(50, 60), Range(90, 100)]
        );
        let moved = e.with_cut_edge_moved(100, 1, false, 45).unwrap();
        assert_eq!(
            moved.cuts(100),
            [Range(20, 30), Range(45, 60), Range(90, 100)]
        );
        // Onto its other edge: gone. Over its neighbour: joined.
        let gone = e.with_cut_edge_moved(100, 0, true, 20).unwrap();
        assert_eq!(gone.cuts(100), [Range(50, 60), Range(90, 100)]);
        let joined = e.with_cut_edge_moved(100, 0, true, 55).unwrap();
        assert_eq!(joined.cuts(100), [Range(20, 60), Range(90, 100)]);
        // The trimmed end, moved back, keeps more.
        let kept = e.with_cut_edge_moved(100, 2, false, 95).unwrap();
        assert_eq!(
            kept.cuts(100),
            [Range(20, 30), Range(50, 60), Range(95, 100)]
        );
        assert!(e.with_cut_edge_moved(100, 3, false, 10).is_none());
    }

    #[test]
    fn cuts_close_together_join() {
        let mut e = EditList::full(100);
        e.cut_joining(20, 30, 5);
        e.cut_joining(33, 40, 5);
        assert_eq!(e.cuts(100), [Range(20, 40)]);
        // Before it, at the very start, and far enough away: kept.
        e.cut_joining(3, 10, 5);
        e.cut_joining(46, 50, 5);
        assert_eq!(e.cuts(100), [Range(0, 10), Range(20, 40), Range(46, 50)]);
        // Only what the cut touches goes; short pieces elsewhere stay.
        let mut e = EditList::full(100);
        e.cut(20, 30);
        e.cut(32, 40);
        e.cut_joining(60, 70, 5);
        assert_eq!(e.cuts(100), [Range(20, 30), Range(32, 40), Range(60, 70)]);
        // A trimmed end that would leave a sliver takes it too.
        e.cut_joining(97, Micros::MAX, 5);
        assert_eq!(e.ranges().last(), Some(&Range(70, 97)));
        e.cut_joining(0, 18, 5);
        assert_eq!(e.ranges()[0], Range(30, 32));
    }

    #[test]
    fn kept_before_counts_kept_time_even_in_cuts() {
        let mut e = EditList::full(100);
        e.cut(20, 30);
        assert_eq!(e.kept_before(10), 10);
        assert_eq!(e.kept_before(25), 20);
        assert_eq!(e.kept_before(40), 30);
        assert_eq!(e.kept_before(100), 90);
        assert_eq!(e.to_output(40), Some(e.kept_before(40)));
    }

    #[test]
    fn cut_middle_splits_range() {
        let mut e = EditList::full(100);
        e.cut(40, 60);
        assert_eq!(e.ranges(), &[Range(0, 40), Range(60, 100)]);
        assert_eq!(e.duration(), 80);
    }

    #[test]
    fn trim_keeps_inner_span() {
        let mut e = EditList::full(100);
        e.cut(40, 60);
        e.trim(10, 90);
        assert_eq!(e.ranges(), &[Range(10, 40), Range(60, 90)]);
    }

    #[test]
    fn cut_spanning_multiple_ranges() {
        let mut e = EditList::full(100);
        e.cut(20, 30);
        e.cut(50, 60);
        e.cut(25, 55);
        assert_eq!(e.ranges(), &[Range(0, 20), Range(60, 100)]);
    }

    #[test]
    fn validity() {
        let mut e = EditList::full(100);
        e.cut(40, 60);
        assert!(e.is_valid(100));
        assert!(!e.is_valid(90));
        let reversed: EditList = serde_json::from_str(r#"{"keep":[[10,5]]}"#).unwrap();
        assert!(!reversed.is_valid(100));
        let overlapping: EditList = serde_json::from_str(r#"{"keep":[[0,50],[40,60]]}"#).unwrap();
        assert!(!overlapping.is_valid(100));
    }

    #[test]
    fn time_mapping_roundtrips() {
        let mut e = EditList::full(100);
        e.cut(40, 60);
        assert_eq!(e.to_source(39), Some(39));
        assert_eq!(e.to_source(40), Some(60));
        assert_eq!(e.to_source(80), None);
        assert_eq!(e.to_output(50), None);
        assert_eq!(e.to_output(60), Some(40));
    }
}
