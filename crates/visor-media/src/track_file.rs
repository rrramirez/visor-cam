//! The file format of recording tracks, shared by the video and audio
//! writers: fragmented MP4 that keeps what was recorded readable if Visor
//! crashes or is killed (see [`crate::recover`]).

use anyhow::bail;
use ffmpeg_next::{self as ffmpeg, Dictionary, format};

/// Writes a track's header. Fragments end at keyframes and at least every
/// half second, and each goes straight to the file rather than waiting in
/// FFmpeg's write buffer, which a still screen's tiny frames could take
/// minutes to fill. Writers [`flush`] when nothing comes for a while.
pub(crate) fn write_header(output: &mut format::context::Output) -> anyhow::Result<()> {
    let mut opts = Dictionary::new();
    opts.set("movflags", "frag_keyframe+empty_moov+default_base_moof");
    opts.set("frag_duration", "500000");
    opts.set("flush_packets", "1");
    let unused = output.write_header_with(opts)?;
    for (option, value) in unused.iter() {
        tracing::warn!(
            option,
            value,
            "the muxer ignored an option, so less may survive a crash"
        );
    }
    Ok(())
}

/// Ends the current fragment and writes it to the file, so it survives a
/// crash even if no more packets come: while the screen is still (capture
/// only delivers changes) or recording is paused.
pub(crate) fn flush(output: &mut format::context::Output) -> anyhow::Result<()> {
    // SAFETY: a valid output context whose header was written; a null
    // packet asks a muxer that allows it (MP4 does) to flush.
    let ret = unsafe { ffmpeg::ffi::av_write_frame(output.as_mut_ptr(), std::ptr::null_mut()) };
    if ret < 0 {
        bail!("flushing a track: {}", ffmpeg::Error::from(ret));
    }
    Ok(())
}
