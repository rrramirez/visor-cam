//! Platform-agnostic types and logic shared by every Visor crate.

pub mod clock;
pub mod config;
pub mod edit;
pub mod layout;
pub mod media;
pub mod project;
pub mod time;

pub use clock::MediaClock;
pub use config::{AudioConfig, CaptureTarget, RecordingConfig, Rect};
pub use edit::{EditList, Range, Removed};
pub use layout::{Layout, Segment};
pub use media::{AudioBuffer, PixelFormat, Plane, PlaneData, VideoFrame};
pub use project::{Blur, CameraOverlay, Chapter, Marker, Project, Track, Tracks};

/// Microseconds. Every timestamp and duration in Visor uses this unit.
pub type Micros = u64;
