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
        planes: vec![
            Plane {
                data: vec![luma; s * h].into(),
                stride: s,
            },
            Plane {
                data: vec![128; s * h / 2].into(),
                stride: s,
            },
        ],
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

#[test]
fn audio_writer_fills_gaps_and_pads_to_end() {
    use visor_core::AudioBuffer;
    use visor_media::AudioWriter;

    let dir = std::env::temp_dir().join(format!("visor-audio-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("mic.m4a");

    // 44.1 kHz mono tone in 10 ms buffers, starting 100 ms late, with a
    // 300 ms hole in the middle (as during a device hiccup).
    let mut writer = AudioWriter::create(&path).unwrap();
    let mut t = 0usize;
    for i in 0..100u64 {
        let pts = 100_000 + i * 10_000;
        if (40..70).contains(&i) {
            continue;
        }
        let samples = (0..441)
            .map(|_| {
                t += 1;
                (t as f32 * 440.0 * std::f32::consts::TAU / 44_100.0).sin() * 0.5
            })
            .collect();
        writer
            .write(
                &AudioBuffer {
                    host_ts: 0,
                    sample_rate: 44_100,
                    channels: 1,
                    samples,
                },
                pts,
            )
            .unwrap();
    }
    writer.finish(2_000_000).unwrap();

    let input = ffmpeg::format::input(&path).unwrap();
    let stream = input.streams().best(ffmpeg::media::Type::Audio).unwrap();
    let params = ffmpeg::codec::context::Context::from_parameters(stream.parameters()).unwrap();
    let audio = params.decoder().audio().unwrap();
    assert_eq!(audio.id(), ffmpeg::codec::Id::AAC);
    assert_eq!((audio.rate(), audio.channels()), (48_000, 2));
    let duration = input.duration() as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE);
    assert!((duration - 2.0).abs() < 0.05, "duration {duration}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn converts_bgra_input() {
    let dir = std::env::temp_dir().join(format!("visor-bgra-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("screen.mp4");
    let (w, h, stride) = (64u32, 48u32, 320usize);
    let mut writer = VideoWriter::create(&path, w, h, 30).unwrap();
    for i in 0..10u64 {
        let frame = VideoFrame {
            host_ts: 0,
            width: w,
            height: h,
            format: PixelFormat::Bgra,
            planes: vec![Plane {
                data: vec![(i * 20) as u8; stride * h as usize].into(),
                stride,
            }],
        };
        writer.write(&frame, i * 33_333, false).unwrap();
    }
    writer.finish(333_330).unwrap();
    let mut input = ffmpeg::format::input(&path).unwrap();
    assert_eq!(input.packets().count(), 10);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn converts_rgbx_like_bgra() {
    let dir = std::env::temp_dir().join(format!("visor-rgbx-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (w, h, stride) = (64u32, 48u32, 256usize);
    // Pure red in each byte order.
    let mut bgra = vec![0u8; stride * h as usize];
    let mut rgbx = vec![0u8; stride * h as usize];
    for px in 0..stride * h as usize / 4 {
        bgra[px * 4..px * 4 + 4].copy_from_slice(&[0, 0, 255, 255]);
        rgbx[px * 4..px * 4 + 4].copy_from_slice(&[255, 0, 0, 255]);
    }
    let mut decoded = Vec::new();
    for (name, format, data) in [
        ("bgra", PixelFormat::Bgra, bgra),
        ("rgbx", PixelFormat::Rgbx, rgbx),
    ] {
        let path = dir.join(format!("{name}.mp4"));
        let mut writer = VideoWriter::create(&path, w, h, 30).unwrap();
        let frame = VideoFrame {
            host_ts: 0,
            width: w,
            height: h,
            format,
            planes: vec![Plane {
                data: data.into(),
                stride,
            }],
        };
        writer.write(&frame, 0, false).unwrap();
        writer.finish(33_333).unwrap();
        let target = Some((ffmpeg::format::Pixel::RGBA, w, h));
        let mut reader = visor_media::decode::VideoReader::open(&path, target).unwrap();
        let (_, f) = reader.next_frame().unwrap().unwrap();
        let px = &f.data(0)[..4];
        decoded.push((px[0], px[1], px[2]));
    }
    for (r, g, b) in &decoded {
        assert!(*r > 230 && *g < 25 && *b < 25, "{decoded:?}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
