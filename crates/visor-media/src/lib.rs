//! Encoding (track writers), decoding, preview playback and export, built on
//! FFmpeg.

mod audio_writer;
mod blur;
pub mod composite;
pub mod dead_time;
pub mod decode;
pub mod export;
pub mod player;
pub mod preview;
pub mod recover;
mod resample;
mod scale;
pub mod tempo;
mod timeline;
mod track_file;
mod writer;

pub use audio_writer::AudioWriter;
pub use export::{ExportFormat, ExportOptions, export};
pub use resample::Resampler;
pub use tempo::Tempo;
pub use writer::VideoWriter;

/// Initializes FFmpeg and limits its console logging to errors.
pub fn init() -> anyhow::Result<()> {
    ffmpeg_next::init()?;
    ffmpeg_next::util::log::set_level(ffmpeg_next::util::log::Level::Error);
    Ok(())
}
