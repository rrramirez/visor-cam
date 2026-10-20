//! Raw media buffers passed from capture backends to encoders.

use crate::Micros;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    /// Bi-planar 4:2:0: a Y plane then an interleaved CbCr plane.
    Nv12,
}

#[derive(Debug, Clone)]
pub struct Plane {
    pub data: Vec<u8>,
    /// Bytes per row; may exceed the visible row width.
    pub stride: usize,
}

#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// Host monotonic clock.
    pub host_ts: Micros,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub planes: Vec<Plane>,
}

#[derive(Debug, Clone)]
pub struct AudioBuffer {
    /// Host monotonic clock.
    pub host_ts: Micros,
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved f32 samples.
    pub samples: Vec<f32>,
}
