//! Blurred areas, hiding what shouldn't be seen (a password, an email) in
//! the export and the editor's preview alike.

use ffmpeg_next::{format, frame};
use visor_core::Blur;

/// How coarse the blur is, as a fraction of the frame's height: about 27
/// pixels at 1080p, so text under it can't be read. A fraction, so the
/// smaller preview looks like the export.
const BLOCK_FRACTION: f64 = 1.0 / 40.0;

fn block_size(height: u32) -> usize {
    ((f64::from(height) * BLOCK_FRACTION).round() as usize).max(2)
}

/// Blurs `blurs`' areas of a YUV 4:2:0 frame.
pub(crate) fn blur_yuv420p(f: &mut frame::Video, blurs: &[&Blur]) {
    debug_assert_eq!(f.format(), format::Pixel::YUV420P);
    let (w, h) = (f.width(), f.height());
    let block = block_size(h);
    for b in blurs {
        let Some((x0, y0, x1, y1)) = b.pixels(w, h) else {
            continue;
        };
        // Whole chroma samples: even edges, rounded outwards.
        let (x0, y0) = (x0 & !1, y0 & !1);
        let (x1, y1) = ((x1 + 1) & !1, (y1 + 1) & !1);
        let stride = f.stride(0);
        blur_area(f.data_mut(0), stride, 1, (x0, y0, x1, y1), block);
        for plane in 1..=2 {
            let stride = f.stride(plane);
            let area = (x0 / 2, y0 / 2, x1 / 2, y1 / 2);
            blur_area(f.data_mut(plane), stride, 1, area, (block / 2).max(1));
        }
    }
}

/// Blurs `blurs`' areas of a tightly packed RGBA image.
pub(crate) fn blur_rgba(rgba: &mut [u8], width: u32, height: u32, blurs: &[&Blur]) {
    let block = block_size(height);
    for b in blurs {
        if let Some(area) = b.pixels(width, height) {
            blur_area(rgba, width as usize * 4, 4, area, block);
        }
    }
}

/// Blurs `(x0, y0, x1, y1)` of an 8-bit image with `channels` interleaved
/// channels per pixel. Each `block`-sized square is averaged, and every
/// pixel takes the blend of the averages around it: it reads as a heavy
/// blur, and nothing finer than a block is left.
fn blur_area(
    data: &mut [u8],
    stride: usize,
    channels: usize,
    (x0, y0, x1, y1): (usize, usize, usize, usize),
    block: usize,
) {
    let (w, h) = (x1 - x0, y1 - y0);
    if w == 0 || h == 0 {
        return;
    }
    let (bw, bh) = (w.div_ceil(block), h.div_ceil(block));
    let mut sums = vec![0u32; bw * bh * channels];
    let mut counts = vec![0u32; bw * bh];
    for y in 0..h {
        let row = &data[(y0 + y) * stride + x0 * channels..][..w * channels];
        let by = y / block;
        for x in 0..w {
            let i = by * bw + x / block;
            counts[i] += 1;
            for c in 0..channels {
                sums[i * channels + c] += u32::from(row[x * channels + c]);
            }
        }
    }
    let averages: Vec<f32> = sums
        .iter()
        .enumerate()
        .map(|(i, &s)| s as f32 / counts[i / channels] as f32)
        .collect();
    // Where a pixel falls between block centres, and how far along.
    let between = |p: usize, n: usize| {
        let f = ((p as f32 + 0.5) / block as f32 - 0.5).max(0.0);
        let i = (f as usize).min(n - 1);
        (i, (i + 1).min(n - 1), (f - i as f32).min(1.0))
    };
    for y in 0..h {
        let (r0, r1, ty) = between(y, bh);
        let row = &mut data[(y0 + y) * stride + x0 * channels..][..w * channels];
        for x in 0..w {
            let (c0, c1, tx) = between(x, bw);
            for c in 0..channels {
                let at = |r: usize, col: usize| averages[(r * bw + col) * channels + c];
                let top = at(r0, c0) + (at(r0, c1) - at(r0, c0)) * tx;
                let bottom = at(r1, c0) + (at(r1, c1) - at(r1, c0)) * tx;
                row[x * channels + c] = (top + (bottom - top) * ty).round() as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blur(x: f64, y: f64, width: f64, height: f64) -> Blur {
        Blur {
            start_us: 0,
            end_us: 1,
            x,
            y,
            width,
            height,
        }
    }

    /// A 1-pixel checkerboard, like small text: as detailed as it gets.
    fn checkerboard(w: usize, h: usize, channels: usize) -> Vec<u8> {
        (0..w * h * channels)
            .map(|i| {
                if ((i / channels) % w + (i / channels) / w).is_multiple_of(2) {
                    20
                } else {
                    230
                }
            })
            .collect()
    }

    #[test]
    fn blurred_area_loses_its_detail_and_the_rest_is_untouched() {
        let (w, h) = (640, 480);
        let original = checkerboard(w, h, 4);
        let mut rgba = original.clone();
        blur_rgba(
            &mut rgba,
            w as u32,
            h as u32,
            &[&blur(0.25, 0.25, 0.5, 0.5)],
        );
        let px = |img: &[u8], x: usize, y: usize| img[(y * w + x) * 4];
        // Inside: every pixel near the mean, none of the pattern left.
        for y in 120..360 {
            for x in 160..480 {
                let v = px(&rgba, x, y);
                assert!((115..=135).contains(&v), "({x}, {y}) is {v}");
            }
        }
        // Outside: as it was, right up to the edge.
        for (x, y) in [(159, 240), (480, 240), (320, 119), (320, 360), (0, 0)] {
            assert_eq!(px(&rgba, x, y), px(&original, x, y), "({x}, {y})");
        }
    }

    #[test]
    fn a_shape_larger_than_a_block_still_shows_through() {
        // Blurred, not blanked: a bright left half stays brighter.
        let (w, h) = (200, 100);
        let mut rgba: Vec<u8> = (0..w * h * 4)
            .map(|i| if (i / 4) % w < w / 2 { 220 } else { 30 })
            .collect();
        blur_rgba(&mut rgba, w as u32, h as u32, &[&blur(0.0, 0.0, 1.0, 1.0)]);
        let px = |x: usize| rgba[(50 * w + x) * 4];
        assert!(px(10) > 200 && px(190) < 50, "{} {}", px(10), px(190));
        assert!(px(95) > px(105), "a smooth edge");
    }

    #[test]
    fn yuv_frames_blur_luma_and_chroma() {
        let (w, h) = (640u32, 480u32);
        let mut f = frame::Video::new(format::Pixel::YUV420P, w, h);
        for plane in 0..3 {
            let (pw, ph) = (
                f.plane_width(plane) as usize,
                f.plane_height(plane) as usize,
            );
            let stride = f.stride(plane);
            let pattern = checkerboard(pw, ph, 1);
            for r in 0..ph {
                f.data_mut(plane)[r * stride..][..pw].copy_from_slice(&pattern[r * pw..][..pw]);
            }
        }
        blur_yuv420p(&mut f, &[&blur(0.0, 0.0, 1.0, 1.0)]);
        for plane in 0..3 {
            let stride = f.stride(plane);
            let mid = f.data(plane)[(f.plane_height(plane) as usize / 2) * stride + 100];
            assert!((110..=140).contains(&mid), "plane {plane}: {mid}");
        }
    }

    #[test]
    fn areas_are_kept_inside_the_frame() {
        assert_eq!(
            blur(-0.5, 0.5, 1.0, 2.0).pixels(100, 50),
            Some((0, 25, 50, 50))
        );
        assert_eq!(blur(1.2, 0.0, 0.5, 0.5).pixels(100, 50), None);
        assert_eq!(blur(f64::NAN, 0.0, 0.5, 0.5).pixels(100, 50), None);
        // Out of range or broken values blur nothing, and don't panic.
        let mut rgba = vec![0u8; 100 * 50 * 4];
        blur_rgba(&mut rgba, 100, 50, &[&blur(f64::INFINITY, 0.0, 1.0, 1.0)]);
    }
}
