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
        self.1 - self.0
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
