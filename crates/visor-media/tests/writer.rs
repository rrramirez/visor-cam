use ffmpeg_next as ffmpeg;
use visor_core::{PixelFormat, Plane, VideoFrame};
use visor_media::VideoWriter;

fn nv12(width: u32, height: u32, stride: usize, luma: u8) -> VideoFrame {
    let (h, s) = (height as usize, stride);
    VideoFrame {
        host_ts: 0,
        width,
        height,
        format: PixelFormat::Nv12,
        planes: vec![Plane { data: vec![luma; s * h], stride: s }, Plane { data: vec![128; s * h / 2], stride: s }],
    }
}

#[test]
fn writes_playable_mp4_with_expected_duration() {
    let dir = std::env::temp_dir().join(format!("visor-writer-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("screen.mp4");

    // Padded stride, as CoreVideo buffers usually have.
    let (w, h, stride) = (320, 240, 384);
    let mut writer = VideoWriter::create(&path, w, h, 30).unwrap();
    for i in 0..60u64 {
        let frame = nv12(w, h, stride, (i * 4) as u8);
        writer.write(&frame, i * 33_333, i == 30).unwrap();
    }
    // Duplicate/out-of-order PTS must be dropped, not error.
    writer.write(&nv12(w, h, stride, 0), 10, false).unwrap();
    // Screen went idle after the last frame; the file must still last 3 s.
    writer.finish(3_000_000).unwrap();

    let input = ffmpeg::format::input(&path).unwrap();
    let stream = input.streams().best(ffmpeg::media::Type::Video).unwrap();
    let params = ffmpeg::codec::context::Context::from_parameters(stream.parameters()).unwrap();
    let video = params.decoder().video().unwrap();
    assert_eq!((video.width(), video.height()), (w, h));
    assert_eq!(video.id(), ffmpeg::codec::Id::H264);

    let mut input = input;
    let packets = input.packets().filter(|(s, _)| s.index() == 0).count();
    assert_eq!(packets, 61, "60 frames plus the held last frame");
    let duration = input.duration() as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE);
    assert!((duration - 3.0).abs() < 0.05, "duration {duration}");

    std::fs::remove_dir_all(&dir).unwrap();
}
