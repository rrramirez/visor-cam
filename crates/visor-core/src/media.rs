//! Raw media buffers passed from capture backends to encoders.

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use crate::Micros;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    /// RGBA byte order; the fourth byte is ignored.
    Rgbx,
    /// Bi-planar 4:2:0: a Y plane then an interleaved CbCr plane.
    Nv12,
}

#[derive(Debug, Clone)]
pub struct Plane {
    pub data: PlaneData,
    /// Bytes per row; may exceed the visible row width.
    pub stride: usize,
}

/// A plane's bytes: owned, or borrowed from a buffer the capture backend
/// keeps alive (a locked CVPixelBuffer on macOS), which saves copying every
/// frame. Cheap to clone; derefs to `[u8]`.
#[derive(Clone)]
pub struct PlaneData(Arc<dyn AsRef<[u8]> + Send + Sync>);

impl PlaneData {
    pub fn new(bytes: impl AsRef<[u8]> + Send + Sync + 'static) -> Self {
        Self(Arc::new(bytes))
    }
}

impl From<Vec<u8>> for PlaneData {
    fn from(bytes: Vec<u8>) -> Self {
        Self::new(bytes)
    }
}

impl Deref for PlaneData {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        (*self.0).as_ref()
    }
}

impl fmt::Debug for PlaneData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PlaneData({} bytes)", self.len())
    }
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
