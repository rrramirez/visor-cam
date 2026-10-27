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
    let n = size as usize;
    // Source column of each output column, mirrored horizontally like a
    // selfie camera.
    let cols: Vec<usize> = (0..n).map(|ox| x0 + (n - 1 - ox) * side / n).collect();
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(size, size);
    for (oy, row) in buf.make_mut_slice().chunks_exact_mut(n).enumerate() {
        let sy = y0 + oy * side / n;
        let luma = &y_plane.data[sy * y_plane.stride..];
        let chroma = &uv_plane.data[(sy / 2) * uv_plane.stride..];
        for (px, &sx) in row.iter_mut().zip(&cols) {
            // BT.709 video range to RGB in 10-bit fixed point: this runs on
            // the UI thread for every pixel of every camera frame.
            let c = (i32::from(luma[sx]) - 16) * 1192;
            let cb = i32::from(chroma[sx & !1]) - 128;
            let cr = i32::from(chroma[sx | 1]) - 128;
            px.r = clamp_u8((c + 1836 * cr) >> 10);
            px.g = clamp_u8((c - 218 * cb - 546 * cr) >> 10);
            px.b = clamp_u8((c + 2163 * cb) >> 10);
            px.a = 255;
        }
    }
    Some(buf)
}

fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
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
                    data: vec![235; 32].into(),
                    stride: 8,
                },
                Plane {
                    data: vec![128; 16].into(),
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
