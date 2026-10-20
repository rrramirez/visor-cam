//! Small helpers shared by the UI modules.

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use visor_core::{Micros, PixelFormat, VideoFrame};
use visor_media::player::PreviewFrame;

/// `m:ss` (or `h:mm:ss`).
pub fn clock(us: Micros) -> String {
    let s = us / 1_000_000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Human file size: `840 KB`, `12.4 MB`, `1.2 GB` (decimal units, like Finder).
pub fn file_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b < 1e6 {
        format!("{:.0} KB", (b / 1e3).max(1.0))
    } else if b < 1e9 {
        format!("{:.1} MB", b / 1e6)
    } else {
        format!("{:.1} GB", b / 1e9)
    }
}

/// `m:ss.d` for the editor.
pub fn clock_tenths(us: Micros) -> String {
    format!("{}.{}", clock(us), us / 100_000 % 10)
}

pub fn preview_image(f: &PreviewFrame) -> Image {
    Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        &f.rgba, f.width, f.height,
    ))
}

/// Center-crops an NV12 frame to a square and scales it to `size` x `size`
/// RGBA (nearest neighbour; it's a small live preview).
pub fn nv12_square_preview(frame: &VideoFrame, size: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    if frame.format != PixelFormat::Nv12 || frame.planes.len() != 2 || size == 0 {
        return None;
    }
    let (w, h) = (frame.width as usize, frame.height as usize);
    let side = w.min(h);
    let (x0, y0) = ((w - side) / 2, (h - side) / 2);
    let (y_plane, uv_plane) = (&frame.planes[0], &frame.planes[1]);
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(size, size);
    let out = buf.make_mut_slice();
    let n = size as usize;
    for oy in 0..n {
        let sy = y0 + oy * side / n;
        for ox in 0..n {
            // Mirror horizontally, like a selfie camera.
            let sx = x0 + (n - 1 - ox) * side / n;
            let luma = f32::from(y_plane.data[sy * y_plane.stride + sx]);
            let uv = (sy / 2) * uv_plane.stride + (sx & !1);
            let cb = f32::from(uv_plane.data[uv]) - 128.0;
            let cr = f32::from(uv_plane.data[uv + 1]) - 128.0;
            let c = (luma - 16.0) * 1.164;
            let px = &mut out[oy * n + ox];
            px.r = (c + 1.793 * cr).clamp(0.0, 255.0) as u8;
            px.g = (c - 0.213 * cb - 0.533 * cr).clamp(0.0, 255.0) as u8;
            px.b = (c + 2.112 * cb).clamp(0.0, 255.0) as u8;
            px.a = 255;
        }
    }
    Some(buf)
}

/// SVG path (viewbox 1000 x 100) of a mirrored waveform from peak levels.
pub fn wave_path(peaks: &[f32]) -> String {
    if peaks.is_empty() {
        return String::new();
    }
    let n = peaks.len() as f32;
    let x = |i: usize| i as f32 * 1000.0 / n;
    let amp = |p: f32| p.clamp(0.0, 1.0).sqrt() * 48.0;
    let mut d = String::from("M 0 50");
    for (i, &p) in peaks.iter().enumerate() {
        d.push_str(&format!(" L {:.1} {:.1}", x(i), 50.0 - amp(p)));
    }
    d.push_str(" L 1000 50");
    for (i, &p) in peaks.iter().enumerate().rev() {
        d.push_str(&format!(" L {:.1} {:.1}", x(i), 50.0 + amp(p)));
    }
    d.push_str(" Z");
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use visor_core::Plane;

    #[test]
    fn file_sizes() {
        assert_eq!(file_size(0), "1 KB");
        assert_eq!(file_size(840_000), "840 KB");
        assert_eq!(file_size(12_400_000), "12.4 MB");
        assert_eq!(file_size(1_234_000_000), "1.2 GB");
    }

    #[test]
    fn formats_clocks() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(83_400_000), "1:23");
        assert_eq!(clock(3_725_000_000), "1:02:05");
        assert_eq!(clock_tenths(83_450_000), "1:23.4");
    }

    #[test]
    fn converts_grey_nv12() {
        let frame = VideoFrame {
            host_ts: 0,
            width: 8,
            height: 4,
            format: PixelFormat::Nv12,
            planes: vec![
                Plane {
                    data: vec![235; 32],
                    stride: 8,
                },
                Plane {
                    data: vec![128; 16],
                    stride: 8,
                },
            ],
        };
        let buf = nv12_square_preview(&frame, 2).unwrap();
        assert!(
            buf.as_slice()
                .iter()
                .all(|p| p.r >= 254 && p.g >= 254 && p.b >= 254)
        );
    }

    #[test]
    fn wave_path_is_closed() {
        let d = wave_path(&[0.0, 1.0]);
        assert!(d.starts_with("M 0 50") && d.ends_with("Z"));
        assert!(d.contains("L 500.0 2.0"));
    }
}
