//! Platform-independent helpers for backends that receive frames faster, larger
//! or in a different shape than the encoder wants (currently Windows Graphics
//! Capture). Kept free of OS APIs so they are unit-tested on every platform.

use visor_core::{Micros, Rect};

/// Rounds down to an even size of at least 2, as H.264 4:2:0 requires.
pub(crate) fn even(n: u32) -> u32 {
    (n & !1).max(2)
}

/// QPC-based 100 ns ticks (WinRT `TimeSpan` / `SystemRelativeTime`, WASAPI
/// QPC positions) to microseconds on the [`crate::host_now`] clock.
pub(crate) fn hns_to_micros(hns: i64) -> Micros {
    (hns.max(0) / 10) as Micros
}

/// A pixel rectangle inside a captured frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PixelBox {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PixelBox {
    /// Clamps the box to a `frame_w` x `frame_h` frame. `None` if nothing is left.
    pub fn clamp_to(self, frame_w: u32, frame_h: u32) -> Option<Self> {
        let x = self.x.min(frame_w);
        let y = self.y.min(frame_h);
        let width = self.width.min(frame_w - x);
        let height = self.height.min(frame_h - y);
        (width > 0 && height > 0).then_some(Self {
            x,
            y,
            width,
            height,
        })
    }
}

/// Converts an area in global logical coordinates to an even-sized pixel box
/// relative to the monitor it is on.
///
/// `monitor` is that monitor's frame in the same global logical coordinates
/// (its physical rect divided by `scale`), `scale` its DPI / 96, and
/// `monitor_px` its size in physical pixels. The result is clamped to the
/// monitor.
pub(crate) fn area_to_pixels(
    area: Rect,
    monitor: Rect,
    scale: f64,
    monitor_px: (u32, u32),
) -> anyhow::Result<PixelBox> {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let left = ((area.x - monitor.x) * scale).round().max(0.0);
    let top = ((area.y - monitor.y) * scale).round().max(0.0);
    let right = ((area.x + area.width - monitor.x) * scale)
        .round()
        .min(f64::from(monitor_px.0));
    let bottom = ((area.y + area.height - monitor.y) * scale)
        .round()
        .min(f64::from(monitor_px.1));
    if right - left < 2.0 || bottom - top < 2.0 {
        anyhow::bail!("capture area {area:?} does not overlap its display {monitor:?}");
    }
    Ok(PixelBox {
        x: left as u32,
        y: top as u32,
        width: (right - left) as u32 & !1,
        height: (bottom - top) as u32 & !1,
    })
}

/// Drops frames so at most `fps` per second pass, keeping a steady cadence.
///
/// Frames are accepted on a fixed schedule; a little early is fine (capture
/// timestamps jitter around the display's refresh), and after an idle period
/// (static screen, no new frames) the schedule restarts from the next frame.
#[derive(Debug, Clone)]
pub(crate) struct FrameLimiter {
    interval: Micros,
    slack: Micros,
    next: Option<Micros>,
}

impl FrameLimiter {
    pub fn new(fps: u32) -> Self {
        let interval = 1_000_000 / Micros::from(fps.max(1));
        Self {
            interval,
            slack: interval / 4,
            next: None,
        }
    }

    /// Whether the frame captured at `ts` should be kept.
    pub fn accept(&mut self, ts: Micros) -> bool {
        match self.next {
            Some(next) if ts + self.slack < next => false,
            prev => {
                let mut next = prev.unwrap_or(ts) + self.interval;
                if next <= ts {
                    next = ts + self.interval;
                }
                self.next = Some(next);
                true
            }
        }
    }
}

/// Copies a `src_w` x `src_h` image (4 bytes per pixel, rows `src_stride`
/// bytes apart) into a tightly packed `dst_w` x `dst_h` image, cropping or
/// padding with opaque black at the right and bottom so the output size never
/// changes when the source does (a window being resized).
pub(crate) fn blit_bgra(
    src: &[u8],
    src_stride: usize,
    (src_w, src_h): (u32, u32),
    (dst_w, dst_h): (u32, u32),
) -> Vec<u8> {
    const BLACK: [u8; 4] = [0, 0, 0, 255];
    let dst_row = dst_w as usize * 4;
    let copy_w = src_w.min(dst_w) as usize * 4;
    let copy_h = src_h.min(dst_h) as usize;
    let mut out = Vec::with_capacity(dst_row * dst_h as usize);
    for y in 0..dst_h as usize {
        let start = out.len();
        if y < copy_h {
            let row = src.get(y * src_stride..y * src_stride + copy_w);
            if let Some(row) = row {
                out.extend_from_slice(row);
            }
        }
        while out.len() < start + dst_row {
            out.extend_from_slice(&BLACK);
        }
    }
    out
}

/// Host timestamp for an audio packet of `frames` frames at `rate` Hz.
///
/// Prefers the device's QPC position (100 ns units) of the packet's first
/// frame; falls back to "now minus the packet's duration" when the position
/// is missing, flagged as bad, or implausibly far from now.
pub(crate) fn audio_packet_ts(
    qpc_hns: u64,
    timestamp_error: bool,
    now: Micros,
    frames: u32,
    rate: u32,
) -> Micros {
    let duration = Micros::from(frames) * 1_000_000 / Micros::from(rate.max(1));
    let fallback = now.saturating_sub(duration);
    if timestamp_error || qpc_hns == 0 {
        return fallback;
    }
    let ts = qpc_hns / 10;
    // A real position is at most a few buffers old and never in the future.
    if ts > now + 10_000 || now.saturating_sub(ts) > 2_000_000 {
        return fallback;
    }
    ts
}

/// Little-endian f32 bytes to samples; a trailing partial sample is dropped.
pub(crate) fn f32_from_le_bytes(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&b| f32::from_le_bytes(b))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn even_sizes() {
        assert_eq!(even(1921), 1920);
        assert_eq!(even(1080), 1080);
        assert_eq!(even(1), 2);
        assert_eq!(even(0), 2);
    }

    #[test]
    fn timespan_to_micros() {
        assert_eq!(hns_to_micros(0), 0);
        assert_eq!(hns_to_micros(10), 1);
        assert_eq!(hns_to_micros(123_456_789), 12_345_678);
        assert_eq!(hns_to_micros(-5), 0);
    }

    #[test]
    fn area_on_primary_at_150_percent() {
        // 2560x1440 physical at 150% = 1706.67x960 logical, at the origin.
        let monitor = rect(0.0, 0.0, 2560.0 / 1.5, 960.0);
        let b =
            area_to_pixels(rect(100.0, 50.0, 401.0, 301.0), monitor, 1.5, (2560, 1440)).unwrap();
        assert_eq!(
            b,
            PixelBox {
                x: 150,
                y: 75,
                width: 602,
                height: 452
            }
        );
    }

    #[test]
    fn area_on_secondary_monitor_is_monitor_relative() {
        // Second monitor at physical x=1920, 100% scale.
        let monitor = rect(1920.0, 0.0, 1920.0, 1080.0);
        let b = area_to_pixels(
            rect(2000.0, 100.0, 640.0, 480.0),
            monitor,
            1.0,
            (1920, 1080),
        )
        .unwrap();
        assert_eq!(
            b,
            PixelBox {
                x: 80,
                y: 100,
                width: 640,
                height: 480
            }
        );
    }

    #[test]
    fn area_is_clamped_to_monitor() {
        let monitor = rect(0.0, 0.0, 1920.0, 1080.0);
        let b = area_to_pixels(
            rect(-10.0, 1000.0, 300.0, 300.0),
            monitor,
            1.0,
            (1920, 1080),
        )
        .unwrap();
        assert_eq!(
            b,
            PixelBox {
                x: 0,
                y: 1000,
                width: 290,
                height: 80
            }
        );
        assert!(area_to_pixels(rect(5000.0, 0.0, 10.0, 10.0), monitor, 1.0, (1920, 1080)).is_err());
    }

    #[test]
    fn pixel_box_clamp() {
        let b = PixelBox {
            x: 100,
            y: 100,
            width: 200,
            height: 200,
        };
        assert_eq!(
            b.clamp_to(250, 400),
            Some(PixelBox {
                x: 100,
                y: 100,
                width: 150,
                height: 200
            })
        );
        assert_eq!(b.clamp_to(100, 400), None);
    }

    #[test]
    fn limiter_halves_60_to_30() {
        let mut l = FrameLimiter::new(30);
        let kept = (0..60)
            .map(|i| i * 1_000_000 / 60)
            .filter(|&ts| l.accept(ts))
            .count();
        assert_eq!(kept, 30);
    }

    #[test]
    fn limiter_passes_matching_rate_with_jitter() {
        let mut l = FrameLimiter::new(60);
        let kept = (0..120u64)
            .map(|i| i * 16_667 + if i % 2 == 0 { 0 } else { 2_000 } - 1_000 * (i % 3))
            .filter(|&ts| l.accept(ts + 10_000))
            .count();
        assert_eq!(kept, 120);
    }

    #[test]
    fn limiter_restarts_after_idle() {
        let mut l = FrameLimiter::new(30);
        assert!(l.accept(0));
        assert!(!l.accept(10_000));
        assert!(l.accept(5_000_000));
        assert!(!l.accept(5_010_000));
        assert!(l.accept(5_033_334));
    }

    #[test]
    fn blit_crops_and_pads() {
        // 2x2 source with a 12-byte stride (1 pixel of padding per row).
        let src: Vec<u8> = (0..24).collect();
        let out = blit_bgra(&src, 12, (2, 2), (3, 1));
        assert_eq!(out, [0, 1, 2, 3, 4, 5, 6, 7, 0, 0, 0, 255]);
        let out = blit_bgra(&src, 12, (2, 2), (1, 3));
        assert_eq!(out, [0, 1, 2, 3, 12, 13, 14, 15, 0, 0, 0, 255]);
        // Short source buffers pad instead of panicking.
        let out = blit_bgra(&src[..8], 12, (2, 2), (2, 2));
        assert_eq!(out.len(), 16);
        assert_eq!(&out[8..], [0, 0, 0, 255, 0, 0, 0, 255]);
    }

    #[test]
    fn audio_ts_prefers_qpc_position() {
        let now = 10_000_000;
        assert_eq!(
            audio_packet_ts(99_900_000, false, now, 480, 48_000),
            9_990_000
        );
        // Error flag, missing or implausible positions fall back to now - duration.
        assert_eq!(
            audio_packet_ts(99_900_000, true, now, 480, 48_000),
            9_990_000
        );
        assert_eq!(audio_packet_ts(0, false, now, 4_800, 48_000), 9_900_000);
        assert_eq!(audio_packet_ts(1, false, now, 4_800, 48_000), 9_900_000);
        assert_eq!(
            audio_packet_ts(200_000_000, false, now, 4_800, 48_000),
            9_900_000
        );
    }

    #[test]
    fn f32_bytes() {
        let bytes: Vec<u8> = [0.5f32, -1.0]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        assert_eq!(f32_from_le_bytes(&bytes), [0.5, -1.0]);
        assert_eq!(f32_from_le_bytes(&bytes[..7]), [0.5]);
    }
}
