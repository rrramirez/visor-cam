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
        let (x1, y1) = (
            ((x1 + 1) & !1).min(w as usize),
            ((y1 + 1) & !1).min(h as usize),
        );
        let luma = Plane::of(f, 0);
        blur_area::<1>(f.data_mut(0), luma, (x0, y0, x1, y1), block);
        for plane in 1..=2 {
            let chroma = Plane::of(f, plane);
            let area = (x0 / 2, y0 / 2, x1 / 2, y1 / 2);
            blur_area::<1>(f.data_mut(plane), chroma, area, (block / 2).max(1));
        }
    }
}

/// Blurs `blurs`' areas of a tightly packed RGBA image.
pub(crate) fn blur_rgba(rgba: &mut [u8], width: u32, height: u32, blurs: &[&Blur]) {
    let block = block_size(height);
    let plane = Plane {
        width: width as usize,
        height: height as usize,
        stride: width as usize * 4,
    };
    for b in blurs {
        if let Some(area) = b.pixels(width, height) {
            blur_area::<4>(rgba, plane, area, block);
        }
    }
}

/// The size of one 8-bit plane, and its bytes per row.
#[derive(Clone, Copy)]
struct Plane {
    width: usize,
    height: usize,
    stride: usize,
}

impl Plane {
    fn of(f: &frame::Video, index: usize) -> Self {
        Self {
            width: f.plane_width(index) as usize,
            height: f.plane_height(index) as usize,
            stride: f.stride(index),
        }
    }
}

/// Blurs `(x0, y0, x1, y1)` of an 8-bit image with `CH` interleaved
/// channels per pixel, fading out over a `block` beyond its edges.
///
/// Each `block`-sized square is averaged first, so nothing finer than a
/// block is left to recover. The averages are smoothed on a grid of
/// [`SAMPLES`] points per block (box blurred twice each way, close to a
/// Gaussian), then spread over the pixels, so no squares show; only that
/// last step and the averaging touch every pixel.
fn blur_area<const CH: usize>(
    data: &mut [u8],
    plane: Plane,
    (x0, y0, x1, y1): (usize, usize, usize, usize),
    block: usize,
) {
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    // The area with its fading edge, inside the image.
    let feather = block;
    let (ex0, ey0) = (x0.saturating_sub(feather), y0.saturating_sub(feather));
    let (ex1, ey1) = (
        (x1 + feather).min(plane.width),
        (y1 + feather).min(plane.height),
    );
    let (w, h, ch) = (ex1 - ex0, ey1 - ey0, CH);
    let at = |y: usize| (ey0 + y) * plane.stride + ex0 * ch;

    // Block averages.
    let (bw, bh) = (w.div_ceil(block), h.div_ceil(block));
    let mut sums = vec![[0u32; CH]; bw * bh];
    for y in 0..h {
        let row = &data[at(y)..][..w * ch];
        let blocks = &mut sums[(y / block) * bw..][..bw];
        for (sum, run) in blocks.iter_mut().zip(row.chunks(block * ch)) {
            let mut acc = [0u32; CH];
            for px in run.as_chunks::<CH>().0 {
                for c in 0..CH {
                    acc[c] += u32::from(px[c]);
                }
            }
            for c in 0..CH {
                sum[c] += acc[c];
            }
        }
    }
    let mut averages: Vec<f32> = sums.iter().flatten().map(|&s| s as f32).collect();
    for by in 0..bh {
        let rows = (h - by * block).min(block);
        for bx in 0..bw {
            let n = (rows * (w - bx * block).min(block)) as f32;
            for v in &mut averages[(by * bw + bx) * ch..][..ch] {
                *v /= n;
            }
        }
    }

    // Smoothed on the finer grid.
    let (sw, sh) = (bw * SAMPLES, bh * SAMPLES);
    let mut grid = resize(&averages, (bw, bh), (sw, sh), ch);
    let mut sums = Vec::new();
    for _ in 0..2 {
        for y in 0..sh {
            for c in 0..ch {
                box_line(&mut grid, y * sw * ch + c, ch, sw, SAMPLES / 2, &mut sums);
            }
        }
        for x in 0..sw {
            for c in 0..ch {
                box_line(&mut grid, x * ch + c, sw * ch, sh, SAMPLES / 2, &mut sums);
            }
        }
    }

    // Spread over the pixels, row by row: across each grid row once, then
    // between the two rows around each pixel row. Fully blurred inside the
    // area, fading out over the feather outside it.
    let scale = block as f32 / SAMPLES as f32;
    let cols: Vec<_> = (0..w).map(|x| between(x, scale, sw)).collect();
    let mut across = vec![0f32; sh * w * ch];
    for gy in 0..sh {
        let src = &grid[gy * sw * ch..][..sw * ch];
        let out = &mut across[gy * w * ch..][..w * ch];
        for (x, &(c0, c1, t)) in cols.iter().enumerate() {
            for c in 0..ch {
                let (a, b) = (src[c0 * ch + c], src[c1 * ch + c]);
                out[x * ch + c] = a + (b - a) * t;
            }
        }
    }
    let distance = |p: usize, lo: usize, hi: usize| {
        let p = p as f32 + 0.5;
        (lo as f32 - p).max(p - hi as f32).max(0.0)
    };
    let dx: Vec<f32> = (0..w).map(|x| distance(ex0 + x, x0, x1)).collect();
    for y in 0..h {
        let (r0, r1, t) = between(y, scale, sh);
        let (a, b) = (
            &across[r0 * w * ch..][..w * ch],
            &across[r1 * w * ch..][..w * ch],
        );
        let dy = distance(ey0 + y, y0, y1);
        let row = &mut data[at(y)..][..w * ch];
        for (x, &dx) in dx.iter().enumerate() {
            let fade = if dx == 0.0 && dy == 0.0 {
                1.0
            } else {
                let f = (1.0 - dx.hypot(dy) / feather as f32).clamp(0.0, 1.0);
                f * f * (3.0 - 2.0 * f)
            };
            for c in 0..ch {
                let i = x * ch + c;
                let blurred = a[i] + (b[i] - a[i]) * t;
                let v = f32::from(row[i]);
                // Never negative, so adding a half and truncating rounds.
                row[i] = (v + (blurred - v) * fade + 0.5) as u8;
            }
        }
    }
}

/// Points per block on the grid the blur is smoothed on.
const SAMPLES: usize = 4;

/// Where pixel `p` falls between the centres of cells `scale` pixels wide,
/// of `n`: the cells before and after it, and how far along.
fn between(p: usize, scale: f32, n: usize) -> (usize, usize, f32) {
    let f = ((p as f32 + 0.5) / scale - 0.5).max(0.0);
    let i = (f as usize).min(n - 1);
    (i, (i + 1).min(n - 1), (f - i as f32).min(1.0))
}

/// `src`, `from` cells across and down, spread over `to` cells: each takes
/// the blend of the source cells around its centre.
fn resize(src: &[f32], from: (usize, usize), to: (usize, usize), ch: usize) -> Vec<f32> {
    let (fw, fh) = from;
    let (tw, th) = to;
    let (sx, sy) = (tw as f32 / fw as f32, th as f32 / fh as f32);
    let mut out = vec![0f32; tw * th * ch];
    for y in 0..th {
        let (r0, r1, ty) = between(y, sy, fh);
        for x in 0..tw {
            let (c0, c1, tx) = between(x, sx, fw);
            for c in 0..ch {
                let v = |r: usize, col: usize| src[(r * fw + col) * ch + c];
                let top = v(r0, c0) + (v(r0, c1) - v(r0, c0)) * tx;
                let bottom = v(r1, c0) + (v(r1, c1) - v(r1, c0)) * tx;
                out[(y * tw + x) * ch + c] = top + (bottom - top) * ty;
            }
        }
    }
    out
}

/// Replaces `len` values of `buf`, `step` apart from `start`, with their
/// mean over `radius` either side (fewer at the ends).
fn box_line(
    buf: &mut [f32],
    start: usize,
    step: usize,
    len: usize,
    radius: usize,
    sums: &mut Vec<f32>,
) {
    sums.clear();
    sums.push(0.0);
    let mut acc = 0.0;
    for i in 0..len {
        acc += buf[start + i * step];
        sums.push(acc);
    }
    for i in 0..len {
        let (a, b) = (i.saturating_sub(radius), (i + radius + 1).min(len));
        buf[start + i * step] = (sums[b] - sums[a]) / (b - a) as f32;
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
        // Outside: as it was beyond the fading edge (a block, 12 pixels).
        for (x, y) in [(147, 240), (492, 240), (320, 107), (320, 372), (0, 0)] {
            assert_eq!(px(&rgba, x, y), px(&original, x, y), "({x}, {y})");
        }
        // The edge fades: a little blur just outside, none further out.
        let changed = |x| {
            (120..360)
                .filter(|&y| px(&rgba, x, y) != px(&original, x, y))
                .count()
        };
        assert!(changed(155) > 0, "just outside the edge");
    }

    #[test]
    fn the_blur_is_smooth_not_blocky() {
        // A sharp edge across the area turns into a gentle slope, with no
        // steps where blocks meet.
        let (w, h) = (640, 480);
        let mut rgba: Vec<u8> = (0..w * h * 4)
            .map(|i| if (i / 4) % w < 330 { 220 } else { 30 })
            .collect();
        blur_rgba(&mut rgba, w as u32, h as u32, &[&blur(0.0, 0.0, 1.0, 1.0)]);
        let row: Vec<i32> = (0..w).map(|x| i32::from(rgba[(240 * w + x) * 4])).collect();
        let steepest = row.windows(2).map(|p| (p[1] - p[0]).abs()).max().unwrap();
        assert!(steepest <= 8, "a step of {steepest}: {row:?}");
        let column: Vec<i32> = (0..h).map(|y| i32::from(rgba[(y * w + 300) * 4])).collect();
        let steepest = column
            .windows(2)
            .map(|p| (p[1] - p[0]).abs())
            .max()
            .unwrap();
        assert!(steepest <= 2, "rows differ: {column:?}");
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
