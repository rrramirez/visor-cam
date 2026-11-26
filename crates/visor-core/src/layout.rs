//! The editor's timeline: the whole recording, laid out clip by clip in
//! the order they play, each with the stretches removed next to it (see
//! [`EditList::removed`]) shown where they would come back.
//!
//! Every moment of the recording has exactly one place on it, so a
//! position on the timeline ("display time", 0 to the recording's length)
//! and a source time map one to one, and nothing on it moves when a
//! stretch is removed or brought back.

use crate::Micros;
use crate::edit::{EditList, Range};

/// A stretch of the timeline: part of clip `clip`, or (not `kept`) a stretch
/// removed next to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// Where it is on the timeline.
    pub at: Micros,
    /// Its part of the recording.
    pub range: Range,
    pub clip: usize,
    pub kept: bool,
}

impl Segment {
    fn end(&self) -> Micros {
        self.at + self.range.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    segments: Vec<Segment>,
    duration: Micros,
}

impl Layout {
    /// The timeline of `edit` over a recording of `duration`.
    pub fn new(edit: &EditList, duration: Micros) -> Self {
        let removed = edit.removed(duration);
        let mut segments = Vec::new();
        let mut at = 0;
        let mut push = |range: Range, clip: usize, kept: bool| {
            segments.push(Segment {
                at,
                range,
                clip,
                kept,
            });
            at += range.len();
        };
        for (clip, &range) in edit.ranges().iter().enumerate() {
            let next_to = |end: bool| {
                removed
                    .iter()
                    .filter(move |r| r.clip == clip && r.end == end)
                    .map(|r| r.range)
            };
            for r in next_to(false) {
                push(r, clip, false);
            }
            push(range, clip, true);
            for r in next_to(true) {
                push(r, clip, false);
            }
        }
        Self { segments, duration }
    }

    /// The timeline's stretches, in order.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// How long the timeline is: the recording's length.
    pub fn duration(&self) -> Micros {
        self.duration
    }

    /// Where source time `t` is on the timeline.
    pub fn display(&self, t: Micros) -> Micros {
        self.segments
            .iter()
            .find(|s| (s.range.0..s.range.1).contains(&t))
            .map_or(self.duration.min(t), |s| s.at + (t - s.range.0))
    }

    /// The source time at timeline position `d`, and whether it plays.
    pub fn source(&self, d: Micros) -> (Micros, bool) {
        let d = d.min(self.duration.saturating_sub(1));
        self.segments
            .iter()
            .find(|s| d < s.end())
            .map_or((d, false), |s| (s.range.0 + (d - s.at), s.kept))
    }

    /// The source time at timeline position `d` if it plays, or else the
    /// first that plays after it on the timeline (the last that plays if
    /// none does).
    pub fn playing_at(&self, d: Micros) -> Micros {
        match self.source(d) {
            (t, true) => t,
            _ => self
                .segments
                .iter()
                .find(|s| s.kept && s.at >= d)
                .map(|s| s.range.0)
                .or_else(|| {
                    let last = self.segments.iter().rfind(|s| s.kept)?;
                    Some(last.range.1.saturating_sub(1))
                })
                .unwrap_or(0),
        }
    }

    /// The parts of the recording that play over timeline `[a, b)`, in
    /// order.
    pub fn kept_pieces(&self, a: Micros, b: Micros) -> Vec<Range> {
        self.segments
            .iter()
            .filter(|s| s.kept)
            .filter_map(|s| {
                let (x, y) = (a.max(s.at), b.min(s.end()));
                (y > x).then(|| Range(s.range.0 + (x - s.at), s.range.0 + (y - s.at)))
            })
            .collect()
    }

    /// Where clip `clip` plays on the timeline.
    pub fn clip_span(&self, clip: usize) -> Option<(Micros, Micros)> {
        let s = self.segments.iter().find(|s| s.kept && s.clip == clip)?;
        Some((s.at, s.end()))
    }

    /// Where clip `clip` is on the timeline with what was removed next to
    /// it, which moves with it.
    pub fn block_span(&self, clip: usize) -> Option<(Micros, Micros)> {
        let mut of = self.segments.iter().filter(|s| s.clip == clip);
        let first = of.next()?;
        let last = of.next_back().unwrap_or(first);
        Some((first.at, last.end()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_timeline_shows_each_clip_with_what_was_removed_next_to_it() {
        let mut e = EditList::full(100);
        e.cut(0, 10);
        e.split(50);
        e.cut(60, 70);
        e.cut(90, 100);
        e.move_clip(0, 3);
        // Plays [50, 60) [70, 90) [10, 50).
        let l = Layout::new(&e, 100);
        let shown: Vec<_> = l
            .segments()
            .iter()
            .map(|s| (s.at, s.range, s.clip, s.kept))
            .collect();
        assert_eq!(
            shown,
            [
                (0, Range(50, 60), 0, true),
                (10, Range(60, 70), 0, false),
                (20, Range(70, 90), 1, true),
                (40, Range(90, 100), 1, false),
                (50, Range(0, 10), 2, false),
                (60, Range(10, 50), 2, true),
            ]
        );
        assert_eq!(l.duration(), 100);
        // One to one.
        for t in 0..100 {
            assert_eq!(l.source(l.display(t)).0, t);
        }
        assert_eq!(l.display(65), 15);
        assert_eq!(l.source(15), (65, false));
        assert_eq!(l.source(25), (75, true));
        // A removed stretch plays on into what comes next on the timeline.
        assert_eq!(l.playing_at(15), 70);
        assert_eq!(l.playing_at(45), 10);
        assert_eq!(l.playing_at(99), 49);
        // Only what plays, in order.
        assert_eq!(
            l.kept_pieces(5, 65),
            [Range(55, 60), Range(70, 90), Range(10, 15)]
        );
        assert_eq!(l.clip_span(2), Some((60, 100)));
        assert_eq!(l.block_span(2), Some((50, 100)));
        assert_eq!(l.block_span(0), Some((0, 20)));
        assert_eq!(l.block_span(3), None);
    }
}
