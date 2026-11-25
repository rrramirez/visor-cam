//! Non-destructive edit list: the ranges of the recording that are kept.
//!
//! Raw tracks are never modified; trimming and cutting only change this list,
//! and export renders it. Each range is a clip, played in the list's order,
//! which needn't be the recording's: clips can be split and moved. Ranges
//! don't overlap and aren't empty; neighbours may touch, as two clips split
//! from one do.

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

    /// Whether the ranges are non-overlapping, non-empty and end by
    /// `duration`, as the edit operations keep them. Lists loaded from disk
    /// are checked with this.
    pub fn is_valid(&self, duration: Micros) -> bool {
        let mut prev_end = 0;
        self.in_recording_order().iter().all(|r| {
            let ok = !r.is_empty() && r.0 >= prev_end && r.1 <= duration;
            prev_end = r.1;
            ok
        })
    }

    /// The ranges in the order they were recorded.
    fn in_recording_order(&self) -> Vec<Range> {
        let mut sorted = self.keep.clone();
        sorted.sort_by_key(|r| r.0);
        sorted
    }

    /// Whether the clips play in the order they were recorded.
    pub fn in_order(&self) -> bool {
        self.keep.windows(2).all(|w| w[0].1 <= w[1].0)
    }

    /// Where clip `index` plays: its output start and end.
    pub fn clip_span(&self, index: usize) -> Option<(Micros, Micros)> {
        let r = self.keep.get(index)?;
        let start = self.keep[..index].iter().map(Range::len).sum();
        Some((start, start + r.len()))
    }

    /// The clip that source time `source` is in.
    pub fn clip_at(&self, source: Micros) -> Option<usize> {
        self.keep.iter().position(|r| (r.0..r.1).contains(&source))
    }

    /// Splits the clip that `source` is inside of in two there. False if it
    /// is in no clip, or at a clip's start.
    pub fn split(&mut self, source: Micros) -> bool {
        match self.clip_at(source) {
            Some(i) if self.keep[i].0 < source => {
                let r = self.keep[i];
                self.keep[i] = Range(r.0, source);
                self.keep.insert(i + 1, Range(source, r.1));
                true
            }
            _ => false,
        }
    }

    /// Moves clip `from` to play before clip `to` (counted before it moves),
    /// or last if `to` is past the end.
    pub fn move_clip(&mut self, from: usize, to: usize) {
        if from >= self.keep.len() {
            return;
        }
        let clip = self.keep.remove(from);
        let to = if to > from { to - 1 } else { to };
        self.keep.insert(to.min(self.keep.len()), clip);
    }

    /// This edit with the start (or `end`) of clip `index` moved to source
    /// time `t`: out to show more of the recording, up to where another
    /// clip's part of it begins, or in to show less. A clip moved onto its
    /// other edge goes away, unless it's the last one. `None` if there's
    /// no such clip.
    pub fn with_clip_edge_moved(
        &self,
        duration: Micros,
        index: usize,
        end: bool,
        t: Micros,
    ) -> Option<Self> {
        let r = *self.keep.get(index)?;
        let others = self.keep.iter().enumerate().filter(|&(i, _)| i != index);
        let mut edit = self.clone();
        if end {
            let limit = others.map(|(_, o)| o.0).filter(|&s| s >= r.1).min();
            edit.keep[index].1 = t.clamp(r.0, limit.unwrap_or(duration).min(duration));
        } else {
            let limit = others.map(|(_, o)| o.1).filter(|&e| e <= r.0).max();
            edit.keep[index].0 = t.clamp(limit.unwrap_or(0), r.1);
        }
        if edit.keep[index].is_empty() {
            if edit.keep.len() == 1 {
                return Some(self.clone());
            }
            edit.keep.remove(index);
        }
        Some(edit)
    }

    /// The source pieces that play over output time `[start, end)`, in the
    /// order they play.
    pub fn pieces(&self, start: Micros, end: Micros) -> Vec<Range> {
        let mut out = Vec::new();
        let mut at = 0;
        for r in &self.keep {
            let (a, b) = (start.max(at), end.min(at + r.len()));
            if b > a {
                out.push(Range(r.0 + (a - at), r.0 + (b - at)));
            }
            at += r.len();
        }
        out
    }

    /// `pieces` (in the order they play) with each one that goes on where
    /// the one before it ends, in the recording as in the video, joined to
    /// it: the parts of clips split but not moved apart.
    pub fn joined(pieces: Vec<Range>) -> Vec<Range> {
        let mut out: Vec<Range> = Vec::with_capacity(pieces.len());
        for p in pieces {
            match out.last_mut() {
                Some(last) if last.1 == p.0 => last.1 = p.1,
                _ => out.push(p),
            }
        }
        out
    }

    /// Removes output time `[start, end)`, which may span several clips.
    pub fn cut_output(&mut self, start: Micros, end: Micros) {
        for p in self.pieces(start, end) {
            self.cut(p.0, p.1);
        }
    }

    /// Keeps only output time `[start, end)`.
    pub fn trim_output(&mut self, start: Micros, end: Micros) {
        self.keep = self.pieces(start, end);
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

    /// Like [`cut`], and also cuts the pieces kept either side of the gap
    /// it leaves (with any cuts it joins) if shorter than `min`, so cuts
    /// close together join rather than leave a flash of video between them.
    /// A cut that removes nothing new changes nothing, and the last kept
    /// piece always stays.
    ///
    /// [`cut`]: Self::cut
    pub fn cut_joining(&mut self, start: Micros, end: Micros, min: Micros) {
        let kept = self.duration();
        self.cut(start, end);
        if self.duration() == kept || self.keep.len() < 2 {
            return;
        }
        // The pieces either side of the gap in the recording, wherever
        // they play.
        let after = self.keep.iter().filter(|r| r.1 > start).map(|r| r.0).min();
        let before = self.keep.iter().filter(|r| r.1 <= start).map(|r| r.1).max();
        for edge in [after.map(|s| (false, s)), before.map(|e| (true, e))]
            .into_iter()
            .flatten()
        {
            let i = self
                .keep
                .iter()
                .position(|r| if edge.0 { r.1 == edge.1 } else { r.0 == edge.1 });
            if let Some(i) = i
                && self.keep[i].len() < min
                && self.keep.len() > 1
            {
                self.keep.remove(i);
            }
        }
    }

    /// The stretches removed from a recording of `duration`, in order: the
    /// gaps between kept ranges, including any trimmed at either end.
    pub fn cuts(&self, duration: Micros) -> Vec<Range> {
        let mut cuts = Vec::new();
        let mut last = 0;
        for r in &self.in_recording_order() {
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

    /// Keep only `[start, end)` (source time), dropping everything outside it.
    pub fn trim(&mut self, start: Micros, end: Micros) {
        self.cut(0, start);
        self.cut(end, Micros::MAX);
    }

    /// Like [`trim`], and also drops the first or last piece it keeps if
    /// shorter than `min` and the trim removed something before or after
    /// it, as [`cut_joining`] does. The last kept piece always stays.
    ///
    /// [`trim`]: Self::trim
    /// [`cut_joining`]: Self::cut_joining
    pub fn trim_joining(&mut self, start: Micros, end: Micros, min: Micros) {
        let (first, last) = match (
            self.keep.iter().map(|r| r.0).min(),
            self.keep.iter().map(|r| r.1).max(),
        ) {
            (Some(first), Some(last)) => (first, last),
            _ => return,
        };
        self.trim(start, end);
        // The first and last pieces in the recording, wherever they play.
        let short = |edit: &Self, last: bool| {
            let i = if last {
                (0..edit.keep.len()).max_by_key(|&i| edit.keep[i].1)
            } else {
                (0..edit.keep.len()).min_by_key(|&i| edit.keep[i].0)
            };
            i.filter(|&i| edit.keep.len() > 1 && edit.keep[i].len() < min)
        };
        if last > end
            && let Some(i) = short(self, true)
        {
            self.keep.remove(i);
        }
        if first < start
            && let Some(i) = short(self, false)
        {
            self.keep.remove(i);
        }
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

    /// Where source time `source` plays: its output time, and inside a
    /// cut, that of the kept time after it in the recording (the end, if
    /// none is).
    pub fn kept_before(&self, source: Micros) -> Micros {
        if let Some(out) = self.to_output(source) {
            return out;
        }
        let next = (0..self.keep.len())
            .filter(|&i| self.keep[i].0 > source)
            .min_by_key(|&i| self.keep[i].0);
        next.and_then(|i| self.clip_span(i))
            .map_or(self.duration(), |(start, _)| start)
    }

    /// Map a source time to output time, or `None` if it was cut.
    pub fn to_output(&self, source: Micros) -> Option<Micros> {
        let mut acc = 0;
        for r in &self.keep {
            if (r.0..r.1).contains(&source) {
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
    fn clips_split_move_and_play_in_their_order() {
        let mut e = EditList::full(100);
        assert!(e.split(40));
        assert!(!e.split(40), "already a clip's start");
        assert!(e.in_order());
        // The second clip first: it plays first.
        e.move_clip(1, 0);
        assert_eq!(e.ranges(), [Range(40, 100), Range(0, 40)]);
        assert!(!e.in_order() && e.is_valid(100));
        assert_eq!(e.to_source(0), Some(40));
        assert_eq!(e.to_source(60), Some(0));
        assert_eq!(e.to_output(10), Some(70));
        assert_eq!(e.clip_span(1), Some((60, 100)));
        // Moved to the end, counting from before it moves.
        e.move_clip(0, 2);
        assert_eq!(e.ranges(), [Range(0, 40), Range(40, 100)]);
    }

    #[test]
    fn pieces_that_go_on_from_each_other_join() {
        let mut e = EditList::full(100);
        e.split(50);
        assert_eq!(
            EditList::joined(e.pieces(40, 60)),
            [Range(40, 60)],
            "split but not moved"
        );
        e.move_clip(1, 0);
        assert_eq!(
            EditList::joined(e.pieces(40, 60)),
            [Range(90, 100), Range(0, 10)]
        );
    }

    #[test]
    fn output_cuts_and_trims_span_clips() {
        let mut e = EditList::full(100);
        e.split(50);
        e.move_clip(1, 0);
        // Plays 50–100, then 0–50: output 40–60 is 90–100 and 0–10.
        assert_eq!(e.pieces(40, 60), [Range(90, 100), Range(0, 10)]);
        let mut cut = e.clone();
        cut.cut_output(40, 60);
        assert_eq!(cut.ranges(), [Range(50, 90), Range(10, 50)]);
        e.trim_output(40, 60);
        assert_eq!(e.ranges(), [Range(90, 100), Range(0, 10)]);
    }

    #[test]
    fn clip_edges_show_more_up_to_other_clips_or_less() {
        let mut e = EditList::full(100);
        e.cut(40, 60);
        e.move_clip(1, 0);
        // [60, 100) then [0, 40): the first clip's start goes back to 40 at
        // most, where the other clip's part ends.
        let more = e.with_clip_edge_moved(100, 0, false, 10).unwrap();
        assert_eq!(more.ranges(), [Range(40, 100), Range(0, 40)]);
        let less = e.with_clip_edge_moved(100, 1, true, 30).unwrap();
        assert_eq!(less.ranges(), [Range(60, 100), Range(0, 30)]);
        let end = e.with_clip_edge_moved(100, 1, true, 90).unwrap();
        assert_eq!(end.ranges(), [Range(60, 100), Range(0, 60)]);
        // Onto its other edge: gone, unless it's the only clip.
        let gone = e.with_clip_edge_moved(100, 1, false, 40).unwrap();
        assert_eq!(gone.ranges(), [Range(60, 100)]);
        let only = gone.with_clip_edge_moved(100, 0, true, 60).unwrap();
        assert_eq!(only, gone);
        assert!(e.with_clip_edge_moved(100, 2, true, 10).is_none());
    }

    #[test]
    fn reordered_clips_join_cuts_and_trim_by_the_recording() {
        let mut e = EditList::full(100);
        e.split(50);
        e.move_clip(1, 0);
        // A short piece left next to a cut, in the recording, goes.
        e.cut_joining(52, 60, 5);
        assert_eq!(e.ranges(), [Range(60, 100), Range(0, 50)]);
        let mut t = e.clone();
        t.trim_joining(3, 98, 5);
        assert_eq!(t.ranges(), [Range(60, 98), Range(3, 50)]);
        // Inside a cut, a time plays where the video goes on after it.
        assert_eq!(e.kept_before(55), 0);
        assert_eq!(e.kept_before(100), 90);
    }

    #[test]
    fn cuts_close_together_join() {
        let mut e = EditList::full(100);
        e.cut_joining(20, 30, 5);
        e.cut_joining(33, 40, 5);
        assert_eq!(e.cuts(100), [Range(20, 40)]);
        // At the very start too; far enough away, kept.
        e.cut_joining(3, 10, 5);
        e.cut_joining(46, 50, 5);
        assert_eq!(e.cuts(100), [Range(0, 10), Range(20, 40), Range(46, 50)]);
        // Only pieces next to the cut go; short ones elsewhere stay.
        let mut e = EditList::full(100);
        e.cut(20, 22);
        e.cut(24, 30);
        e.cut_joining(60, 70, 5);
        assert_eq!(e.cuts(100), [Range(20, 22), Range(24, 30), Range(60, 70)]);
        // Next to the gap a cut joins, not only its own edges.
        let mut joined = e.clone();
        joined.cut_joining(25, 50, 5);
        assert_eq!(joined.cuts(100), [Range(20, 50), Range(60, 70)]);
        // A cut that removes nothing new leaves short pieces alone.
        let mut same = e.clone();
        same.cut_joining(24, 30, 5);
        assert_eq!(same, e);
        // The last kept piece stays, however short: a short trim works.
        let mut e = EditList::full(100);
        e.cut_joining(0, 10, 5);
        e.cut_joining(12, Micros::MAX, 5);
        assert_eq!(e.ranges(), [Range(10, 12)]);
        let mut e = EditList::full(100);
        e.cut(2, 50);
        e.cut_joining(50, 100, 5);
        assert_eq!(e.ranges(), [Range(0, 2)]);
    }

    #[test]
    fn trims_drop_short_ends_but_never_everything() {
        let mut e = EditList::full(100);
        e.cut(20, 22);
        e.cut(24, 30);
        // Short pieces at either end of the trim go.
        let mut t = e.clone();
        t.trim_joining(18, 50, 5);
        assert_eq!(t.ranges(), [Range(22, 24), Range(30, 50)]);
        let mut t = e.clone();
        t.trim_joining(10, 31, 5);
        assert_eq!(t.ranges(), [Range(10, 20), Range(22, 24)]);
        // Unless that is all the trim keeps.
        let mut t = e.clone();
        t.trim_joining(21, 25, 5);
        assert_eq!(t.ranges(), [Range(22, 24)]);
        let mut t = EditList::full(100);
        t.trim_joining(10, 12, 5);
        assert_eq!(t.ranges(), [Range(10, 12)]);
        // Nothing removed on a side: its piece stays, however short.
        let mut t = e.clone();
        t.cut(2, 20);
        t.trim_joining(0, 50, 5);
        assert_eq!(t.ranges(), [Range(0, 2), Range(22, 24), Range(30, 50)]);
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
