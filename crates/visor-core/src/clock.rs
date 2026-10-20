//! Pause-aware recording clock.
//!
//! Every capture backend reports buffers in host-clock time. `MediaClock` maps
//! those to output PTS: `host - t0 - paused_total`. Buffers that arrive while
//! paused map to `None` and are dropped, so tracks stay gapless and in sync
//! across pause/resume.

use crate::Micros;

#[derive(Debug, Clone)]
pub struct MediaClock {
    t0: Micros,
    paused_total: Micros,
    paused_at: Option<Micros>,
    /// Pauses already completed, as (host_start, host_end). Lets late-arriving
    /// buffers that were captured during a past pause still be dropped.
    pauses: Vec<(Micros, Micros)>,
}

impl MediaClock {
    pub fn start(host_now: Micros) -> Self {
        Self {
            t0: host_now,
            paused_total: 0,
            paused_at: None,
            pauses: Vec::new(),
        }
    }

    /// Number of completed pauses. Video tracks force a keyframe when it changes.
    pub fn epoch(&self) -> usize {
        self.pauses.len()
    }

    pub fn is_paused(&self) -> bool {
        self.paused_at.is_some()
    }

    pub fn pause(&mut self, host_now: Micros) {
        if self.paused_at.is_none() {
            self.paused_at = Some(host_now);
        }
    }

    pub fn resume(&mut self, host_now: Micros) {
        if let Some(at) = self.paused_at.take() {
            let end = host_now.max(at);
            self.paused_total += end - at;
            self.pauses.push((at, end));
        }
    }

    /// Output PTS for a buffer captured at `host_ts`, or `None` if it falls
    /// before the start or inside a pause.
    pub fn pts(&self, host_ts: Micros) -> Option<Micros> {
        if host_ts < self.t0 {
            return None;
        }
        if matches!(self.paused_at, Some(at) if host_ts >= at) {
            return None;
        }
        let mut paused_before = 0;
        for &(start, end) in &self.pauses {
            if host_ts < start {
                break;
            }
            if host_ts < end {
                return None;
            }
            paused_before += end - start;
        }
        Some(host_ts - self.t0 - paused_before)
    }

    /// Recorded (non-paused) duration as of `host_now`.
    pub fn elapsed(&self, host_now: Micros) -> Micros {
        let now = self.paused_at.unwrap_or(host_now);
        now.saturating_sub(self.t0)
            .saturating_sub(self.paused_total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_without_pauses() {
        let c = MediaClock::start(1_000);
        assert_eq!(c.pts(999), None);
        assert_eq!(c.pts(1_000), Some(0));
        assert_eq!(c.pts(1_500), Some(500));
    }

    #[test]
    fn drops_and_shifts_around_pause() {
        let mut c = MediaClock::start(0);
        c.pause(100);
        assert_eq!(c.pts(150), None);
        assert_eq!(c.elapsed(10_000), 100);
        c.resume(400);
        assert_eq!(c.pts(99), Some(99));
        assert_eq!(c.pts(250), None, "late buffer from inside the pause");
        assert_eq!(c.pts(400), Some(100));
        assert_eq!(c.pts(500), Some(200));
        assert_eq!(c.elapsed(500), 200);
    }

    #[test]
    fn multiple_pauses_accumulate() {
        let mut c = MediaClock::start(0);
        c.pause(10);
        c.resume(20);
        c.pause(30);
        c.resume(50);
        assert_eq!(c.pts(25), Some(15));
        assert_eq!(c.pts(60), Some(30));
    }
}
