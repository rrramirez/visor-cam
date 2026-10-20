//! Camera bubble compositing onto YUV 4:2:0 frames.

use ffmpeg_next::{format, frame};
use visor_core::CameraOverlay;
use visor_core::project::BubbleShape;

/// Placement and alpha mask of the bubble in an output frame.
pub struct Bubble {
    /// Diameter (or side) in output pixels; even.
    pub size: usize,
    pub x: usize,
    pub y: usize,
    mirror: bool,
    mask: Vec<u8>,
}

impl Bubble {
    /// `None` when the overlay is hidden or would be too small to see.
    pub fn new(overlay: &CameraOverlay, out_w: u32, out_h: u32) -> Option<Self> {
        if !overlay.visible {
            return None;
        }
        let (w, h) = (out_w as f64, out_h as f64);
        let size = ((overlay.size * w).round() as usize).min(out_w.min(out_h) as usize) & !1;
        if size < 16 {
            return None;
        }
        let max_x = out_w as usize - size;
        let max_y = out_h as usize - size;
        let x = ((overlay.x * w).round().max(0.0) as usize).min(max_x) & !1;
        let y = ((overlay.y * h).round().max(0.0) as usize).min(max_y) & !1;
        Some(Self {
            size,
            x,
            y,
            mirror: overlay.mirror,
            mask: mask(overlay.shape, size),
        })
    }

    /// Size to scale camera frames to before [`blend`](Self::blend): the
    /// bubble's height, keeping the camera's aspect (center-cropped later).
    pub fn camera_scale(&self, cam_w: u32, cam_h: u32) -> (u32, u32) {
        let h = self.size as u32;
        let w = ((u64::from(h) * u64::from(cam_w) / u64::from(cam_h.max(1))) as u32).max(h);
        (w + (w & 1), h)
    }

    /// Blends `cam` (YUV420P, sized by [`camera_scale`](Self::camera_scale))
    /// into `dst` (YUV420P).
    pub fn blend(&self, dst: &mut frame::Video, cam: &frame::Video) {
        debug_assert_eq!(dst.format(), format::Pixel::YUV420P);
        debug_assert_eq!(cam.format(), format::Pixel::YUV420P);
        let d = self.size;
        let crop_x = ((cam.width() as usize).saturating_sub(d) / 2) & !1;

        let (cam_stride, dst_stride) = (cam.stride(0), dst.stride(0));
        let cam_y = cam.data(0).to_vec();
        let dst_y = dst.data_mut(0);
        for r in 0..d {
            let src = &cam_y[r * cam_stride + crop_x..][..d];
            let out = &mut dst_y[(self.y + r) * dst_stride + self.x..][..d];
            let m = &self.mask[r * d..][..d];
            for c in 0..d {
                let s = if self.mirror { src[d - 1 - c] } else { src[c] };
                out[c] = mix(s, out[c], m[c]);
            }
        }

        let half = d / 2;
        for plane in 1..=2 {
            let (cam_stride, dst_stride) = (cam.stride(plane), dst.stride(plane));
            let src_plane = cam.data(plane).to_vec();
            let out_plane = dst.data_mut(plane);
            for r in 0..half {
                let src = &src_plane[r * cam_stride + crop_x / 2..][..half];
                let out = &mut out_plane[(self.y / 2 + r) * dst_stride + self.x / 2..][..half];
                for c in 0..half {
                    let i = 2 * r * d + 2 * c;
                    let a = (u16::from(self.mask[i])
                        + u16::from(self.mask[i + 1])
                        + u16::from(self.mask[i + d])
                        + u16::from(self.mask[i + d + 1]))
                        / 4;
                    let s = if self.mirror {
                        src[half - 1 - c]
                    } else {
                        src[c]
                    };
                    out[c] = mix(s, out[c], a as u8);
                }
            }
        }
    }
}

fn mix(fg: u8, bg: u8, alpha: u8) -> u8 {
    let (fg, bg, a) = (u32::from(fg), u32::from(bg), u32::from(alpha));
    ((fg * a + bg * (255 - a) + 127) / 255) as u8
}

/// Anti-aliased alpha mask, row-major `size * size`.
pub fn mask(shape: BubbleShape, size: usize) -> Vec<u8> {
    let half = size as f64 / 2.0;
    let corner = match shape {
        BubbleShape::Circle => half,
        BubbleShape::RoundedRect => size as f64 * 0.18,
    };
    let mut out = Vec::with_capacity(size * size);
    for r in 0..size {
        for c in 0..size {
            // Signed distance from the pixel center to a rounded square.
            let px = (c as f64 + 0.5 - half).abs() - (half - corner);
            let py = (r as f64 + 0.5 - half).abs() - (half - corner);
            let outside = px.max(0.0).hypot(py.max(0.0)) + px.max(py).min(0.0) - corner;
            let alpha = (0.5 - outside).clamp(0.0, 1.0);
            out.push((alpha * 255.0).round() as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circle_mask_is_opaque_inside_and_clear_at_corners() {
        let m = mask(BubbleShape::Circle, 64);
        assert_eq!(m[32 * 64 + 32], 255);
        assert_eq!(m[0], 0);
        assert_eq!(m[63 * 64 + 63], 0);
        // Edge midpoint is partially covered.
        assert!(m[32 * 64] > 0);
    }

    #[test]
    fn rounded_rect_mask_fills_more_than_circle() {
        let sum = |m: Vec<u8>| m.iter().map(|&v| u64::from(v)).sum::<u64>();
        assert!(sum(mask(BubbleShape::RoundedRect, 64)) > sum(mask(BubbleShape::Circle, 64)));
    }

    #[test]
    fn bubble_is_clamped_inside_frame_and_even() {
        let overlay = CameraOverlay {
            visible: true,
            shape: BubbleShape::Circle,
            x: 0.95,
            y: 0.95,
            size: 0.2,
            mirror: true,
        };
        let b = Bubble::new(&overlay, 1920, 1080).unwrap();
        assert_eq!(b.size, 384);
        assert!(b.x + b.size <= 1920 && b.y + b.size <= 1080);
        assert_eq!((b.x % 2, b.y % 2), (0, 0));
        assert_eq!(b.camera_scale(1280, 720), (682, 384));
        let hidden = CameraOverlay {
            visible: false,
            ..overlay
        };
        assert!(Bubble::new(&hidden, 1920, 1080).is_none());
    }

    #[test]
    fn blend_writes_camera_inside_bubble_only() {
        let overlay = CameraOverlay {
            visible: true,
            shape: BubbleShape::Circle,
            x: 0.0,
            y: 0.0,
            size: 0.5,
            mirror: false,
        };
        let b = Bubble::new(&overlay, 64, 64).unwrap();
        let mut dst = frame::Video::new(format::Pixel::YUV420P, 64, 64);
        let (cw, ch) = b.camera_scale(64, 32);
        let mut cam = frame::Video::new(format::Pixel::YUV420P, cw, ch);
        for p in 0..3 {
            dst.data_mut(p).fill(10);
            cam.data_mut(p).fill(240);
        }
        b.blend(&mut dst, &cam);
        let y = |f: &frame::Video, r: usize, c: usize| f.data(0)[r * f.stride(0) + c];
        assert_eq!(y(&dst, 16, 16), 240, "bubble center");
        assert_eq!(y(&dst, 0, 0), 10, "outside circle corner");
        assert_eq!(y(&dst, 48, 48), 10, "outside bubble");
    }

    #[test]
    fn mirror_flips_the_camera_horizontally() {
        let left_of_bubble = |mirror: bool| {
            let overlay = CameraOverlay {
                visible: true,
                shape: BubbleShape::Circle,
                x: 0.0,
                y: 0.0,
                size: 0.5,
                mirror,
            };
            let b = Bubble::new(&overlay, 64, 64).unwrap();
            let mut dst = frame::Video::new(format::Pixel::YUV420P, 64, 64);
            let (cw, ch) = b.camera_scale(64, 32);
            let mut cam = frame::Video::new(format::Pixel::YUV420P, cw, ch);
            // Camera: dark left half, bright right half.
            let stride = cam.stride(0);
            for r in 0..ch as usize {
                for c in 0..cw as usize {
                    cam.data_mut(0)[r * stride + c] = if c < cw as usize / 2 { 20 } else { 220 };
                }
            }
            b.blend(&mut dst, &cam);
            dst.data(0)[16 * dst.stride(0) + 8]
        };
        assert_eq!(left_of_bubble(false), 20);
        assert_eq!(left_of_bubble(true), 220);
    }
}
