mod common;

use std::sync::atomic::AtomicBool;

use common::*;
use ffmpeg_next as ffmpeg;
use visor_media::decode::{AudioReader, VideoReader};
use visor_media::export::{AudioCleanup, save_thumbnail};
use visor_media::{ExportFormat, ExportOptions, export};

fn probe(path: &std::path::Path) -> (f64, Vec<ffmpeg::codec::Id>) {
    let input = ffmpeg::format::input(path).unwrap();
    let duration = input.duration() as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE);
    let codecs = input.streams().map(|s| s.parameters().id()).collect();
    (duration, codecs)
}

/// The file's chapters: start and end seconds, and title.
fn chapters(path: &std::path::Path) -> Vec<(f64, f64, String)> {
    let input = ffmpeg::format::input(path).unwrap();
    input
        .chapters()
        .map(|c| {
            let s = |t: i64| t as f64 * f64::from(c.time_base());
            let title = c.metadata().get("title").unwrap_or_default().to_string();
            (
                (s(c.start()) * 100.0).round() / 100.0,
                (s(c.end()) * 100.0).round() / 100.0,
                title,
            )
        })
        .collect()
}

fn marker(at_us: u64, text: &str) -> visor_core::Marker {
    visor_core::Marker {
        at_us,
        text: text.into(),
    }
}

/// Mean luma of the top half (away from the bottom-left camera bubble).
fn top_luma(f: &ffmpeg::frame::Video) -> f64 {
    let (w, h, stride) = (f.width() as usize, f.height() as usize / 2, f.stride(0));
    let sum: u64 = (0..h)
        .flat_map(|y| f.data(0)[y * stride..y * stride + w].iter())
        .map(|&v| u64::from(v))
        .sum();
    sum as f64 / (w * h) as f64
}

#[test]
fn exports_mp4_with_cuts_and_bubble() {
    let (dir, mut project) = project("export-mp4", 6);
    project.edit.cut(1_000_000, 2_000_000);
    project.edit.cut(3_500_000, 4_000_000);
    // A NUL, which C strings can't hold, is left out.
    project.markers = vec![marker(2_500_000, "Step\0 2")];
    let out = dir.join("out.mp4");
    let mut last = 0.0;
    export(
        &dir,
        &project,
        &out,
        &ExportOptions::new(ExportFormat::Mp4),
        &mut |p| last = p,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!((last - 1.0).abs() < 1e-6);

    let (duration, codecs) = probe(&out);
    assert!((duration - 4.5).abs() < 0.08, "duration {duration}");
    // Then the chapters' text track (as QuickTime has them), and the cover art.
    assert_eq!(
        codecs,
        [
            ffmpeg::codec::Id::H264,
            ffmpeg::codec::Id::AAC,
            ffmpeg::codec::Id::BIN_DATA,
            ffmpeg::codec::Id::MJPEG
        ]
    );
    // The marker at 2.5 s, 1 s of it cut before, starts a chapter at 1.5 s.
    assert_eq!(
        chapters(&out),
        [
            (0.0, 1.5, "export-mp4".to_string()),
            (1.5, 4.5, "Step 2".to_string())
        ]
    );

    // Output 1.2 s is source 2.2 s (frame 66); output 3.2 s is source 4.7 s (frame 141).
    let mut reader =
        VideoReader::open(&out, Some((ffmpeg::format::Pixel::YUV420P, 640, 360))).unwrap();
    for (out_t, src_frame) in [(1_200_000u64, 66u64), (3_200_000, 141), (500_000, 15)] {
        reader.seek(out_t).unwrap();
        let (_, f) = reader.frame_at(out_t).unwrap().unwrap();
        let expected = f64::from(luma(src_frame));
        assert!(
            (top_luma(&f) - expected).abs() < 8.0,
            "at {out_t}: {} vs {expected}",
            top_luma(&f)
        );
    }
    // The bubble (camera chroma 200) is composited bottom-left, not top-right.
    reader.seek(0).unwrap();
    let (_, f) = reader.frame_at(0).unwrap().unwrap();
    let u = |x: usize, y: usize| f.data(1)[y * f.stride(1) + x];
    assert!(u(50, 155) > 180, "bubble chroma {}", u(50, 155));
    assert!(u(300, 20) < 140, "screen chroma {}", u(300, 20));
    common::cleanup(&dir);
}

#[test]
fn exports_webm_vp9_and_av1() {
    let (dir, mut project) = project("export-webm", 3);
    project.edit.trim(500_000, 2_500_000);
    project.markers = vec![marker(1_500_000, "Step 2")];
    for av1 in [false, true] {
        let out = dir.join(format!("out-{av1}.webm"));
        let opts = ExportOptions::new(ExportFormat::WebM { av1 });
        export(
            &dir,
            &project,
            &out,
            &opts,
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        let (duration, codecs) = probe(&out);
        assert!(
            (duration - 2.0).abs() < 0.1,
            "av1={av1} duration {duration}"
        );
        let video = if av1 {
            ffmpeg::codec::Id::AV1
        } else {
            ffmpeg::codec::Id::VP9
        };
        assert_eq!(codecs, [video, ffmpeg::codec::Id::OPUS]);
        assert_eq!(
            chapters(&out),
            [
                (0.0, 1.0, "export-webm".to_string()),
                (1.0, 2.0, "Step 2".to_string())
            ]
        );
    }
    common::cleanup(&dir);
}

#[test]
fn exports_gif_and_honours_cancel() {
    let (dir, project) = project("export-gif", 2);
    let out = dir.join("out.gif");
    export(
        &dir,
        &project,
        &out,
        &ExportOptions::new(ExportFormat::Gif),
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    let (duration, codecs) = probe(&out);
    assert_eq!(codecs, [ffmpeg::codec::Id::GIF]);
    assert!((duration - 2.0).abs() < 0.15, "duration {duration}");

    let cancelled = dir.join("cancelled.mp4");
    let err = export(
        &dir,
        &project,
        &cancelled,
        &ExportOptions::new(ExportFormat::Mp4),
        &mut |_| {},
        &AtomicBool::new(true),
    );
    assert!(err.is_err());
    assert!(!cancelled.exists(), "partial file must be removed");
    common::cleanup(&dir);
}

#[test]
fn exports_sped_up_video_and_audio() {
    let (dir, mut project) = project("export-speed", 6);
    project.edit.cut(1_000_000, 2_000_000);
    project.speed = 1.5;
    let out = dir.join("fast.mp4");
    export(
        &dir,
        &project,
        &out,
        &ExportOptions::new(ExportFormat::Mp4),
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();

    // 5 s kept, at 1.5x.
    let (duration, _) = probe(&out);
    assert!((duration - 5.0 / 1.5).abs() < 0.08, "duration {duration}");
    let input = ffmpeg::format::input(&out).unwrap();
    for s in input.streams() {
        let secs = s.duration() as f64 * f64::from(s.time_base());
        assert!(
            (secs - 5.0 / 1.5).abs() < 0.1,
            "{:?} lasts {secs}",
            s.parameters().id()
        );
    }

    // Output 2.0 s is edit 3.0 s, which is source 4.0 s (frame 120).
    let mut reader =
        VideoReader::open(&out, Some((ffmpeg::format::Pixel::YUV420P, 640, 360))).unwrap();
    reader.seek(2_000_000).unwrap();
    let (_, f) = reader.frame_at(2_000_000).unwrap().unwrap();
    let expected = f64::from(luma(120));
    assert!(
        (top_luma(&f) - expected).abs() < 8.0,
        "{} vs {expected}",
        top_luma(&f)
    );
    common::cleanup(&dir);
}

/// The cover art's JPEG bytes, if the file has one.
fn cover(path: &std::path::Path) -> Option<Vec<u8>> {
    let mut input = ffmpeg::format::input(path).unwrap();
    let index = input
        .streams()
        .find(|s| {
            s.disposition()
                .contains(ffmpeg::format::stream::Disposition::ATTACHED_PIC)
        })?
        .index();
    input
        .packets()
        .find(|(s, _)| s.index() == index)
        .and_then(|(_, p)| p.data().map(<[u8]>::to_vec))
}

/// A JPEG decoded.
fn decode_jpeg(jpeg: &[u8]) -> ffmpeg::frame::Video {
    let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::MJPEG).unwrap();
    let mut decoder = ffmpeg::codec::context::Context::new_with_codec(codec)
        .decoder()
        .video()
        .unwrap();
    decoder.send_packet(&ffmpeg::Packet::copy(jpeg)).unwrap();
    decoder.send_eof().unwrap();
    let mut f = ffmpeg::frame::Video::empty();
    decoder.receive_frame(&mut f).unwrap();
    f
}

/// A video-range luma level as JPEG (full range) stores it.
fn full_range(y: u8) -> f64 {
    (f64::from(y) - 16.0) * 255.0 / 219.0
}

fn assert_jpeg_full_range(f: &ffmpeg::frame::Video) {
    // JPEG decodes to the "J" (full range) formats, or says so outright.
    let full = f.format() == ffmpeg::format::Pixel::YUVJ420P
        || f.color_range() == ffmpeg::color::Range::JPEG;
    assert!(full, "{:?} {:?}", f.format(), f.color_range());
}

#[test]
fn mp4_cover_is_the_chosen_thumbnail() {
    let (dir, mut project) = project("export-cover", 4);
    // Frame 75: the luma there differs from the first frame's.
    project.thumbnail_us = Some(75 * FRAME_US + 1000);
    let out = dir.join("out.mp4");
    let opts = ExportOptions::new(ExportFormat::Mp4);
    export(
        &dir,
        &project,
        &out,
        &opts,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    let jpeg = cover(&out).expect("no cover art");
    assert_eq!(&jpeg[..2], [0xFF, 0xD8]);
    // Checking what the images show needs a JPEG decoder, which the app
    // itself doesn't: FFmpeg builds made for packaging leave it out.
    let can_decode = ffmpeg::decoder::find(ffmpeg::codec::Id::MJPEG).is_some();
    if !can_decode {
        eprintln!("this FFmpeg can't decode JPEG: only checking that images are written");
    }
    if can_decode {
        // Most of the frame is screen (the bubble covers the bottom left).
        let f = decode_jpeg(&jpeg);
        assert_jpeg_full_range(&f);
        let l = mean_luma(&f);
        let expected = full_range(luma(75));
        assert!((l - expected).abs() < 25.0, "cover luma {l} vs {expected}");
    }

    // Cut away, the thumbnail falls back to the first kept frame.
    project.edit.cut(2_000_000, 3_000_000);
    project.edit.trim(500_000, 4_000_000);
    assert_eq!(visor_media::export::thumbnail_time(&project), 500_000);

    let image = dir.join("thumb.jpg");
    save_thumbnail(&dir, &project, &image).unwrap();
    let saved = std::fs::read(&image).unwrap();
    assert_eq!(&saved[..2], [0xFF, 0xD8]);
    if can_decode {
        let f = decode_jpeg(&saved);
        assert_jpeg_full_range(&f);
        let expected = full_range(luma(15));
        assert!(
            (mean_luma(&f) - expected).abs() < 25.0,
            "saved luma vs {expected}"
        );
        // Full size.
        assert_eq!((f.width(), f.height()), (640, 360));

        // Video black (16) must come out as JPEG black (0), not grey.
        project.edit = visor_core::EditList::full(project.duration_us);
        project.thumbnail_us = Some(0);
        save_thumbnail(&dir, &project, &image).unwrap();
        let black = top_luma(&decode_jpeg(&std::fs::read(&image).unwrap()));
        assert!(black < 4.0, "black came out as {black}");
    }
    common::cleanup(&dir);
}

/// RMS level of the first audio stream.
fn rms(path: &std::path::Path) -> (f64, f64) {
    let mut reader = AudioReader::open(path).unwrap();
    let (mut sum, mut n) = (0f64, 0usize);
    while let Some((_, chunk)) = reader.next_chunk().unwrap() {
        sum += chunk.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
        n += chunk.len();
    }
    ((sum / n.max(1) as f64).sqrt(), n as f64 / 2.0 / 48_000.0)
}

#[test]
fn audio_cleanup_keeps_length_and_sets_loudness() {
    let (dir, project) = project("export-cleanup", 5);
    let plain = dir.join("plain.mp4");
    let cleaned = dir.join("cleaned.mp4");
    let mut opts = ExportOptions::new(ExportFormat::Mp4);
    export(
        &dir,
        &project,
        &plain,
        &opts,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    opts.audio = AudioCleanup {
        denoise: true,
        normalize: true,
    };
    export(
        &dir,
        &project,
        &cleaned,
        &opts,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();

    let (loud, plain_secs) = rms(&plain);
    let (level, secs) = rms(&cleaned);
    assert!(
        (secs - plain_secs).abs() < 0.05,
        "{secs} s vs {plain_secs} s"
    );
    // A 0.6 amplitude tone (about -7 dBFS RMS) comes down to about -16.
    let db = 20.0 * level.log10();
    assert!(
        level < loud * 0.6 && (-22.0..-11.0).contains(&db),
        "{db:.1} dBFS"
    );
    common::cleanup(&dir);
}

/// Spread of luma (standard deviation) in a frame's `(x0, y0, x1, y1)`.
fn luma_spread(f: &ffmpeg::frame::Video, (x0, y0, x1, y1): (usize, usize, usize, usize)) -> f64 {
    let stride = f.stride(0);
    let values: Vec<f64> = (y0..y1)
        .flat_map(|y| (x0..x1).map(move |x| (y, x)))
        .map(|(y, x)| f64::from(f.data(0)[y * stride + x]))
        .collect();
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64).sqrt()
}

#[test]
fn blurred_areas_hide_detail_while_they_last() {
    // A fine checkerboard, like small text, for two seconds.
    let (w, h) = (640u32, 360u32);
    let (dir, mut project) = checkerboard_project("export-blur", w, h, 2);
    // The left half, for the second second.
    project.blurs = vec![visor_core::Blur {
        start_us: 1_000_000,
        end_us: 2_000_000,
        x: 0.0,
        y: 0.0,
        width: 0.5,
        height: 1.0,
    }];
    let (left, right) = ((20, 20, 300, 340), (340, 20, 620, 340));
    let at = |t| decode_jpeg(&visor_media::export::frame_jpeg(&dir, &project, t, h).unwrap());

    let before = at(500_000);
    assert!(
        luma_spread(&before, left) > 40.0,
        "{}",
        luma_spread(&before, left)
    );
    let during = at(1_500_000);
    assert!(
        luma_spread(&during, left) < 8.0,
        "{}",
        luma_spread(&during, left)
    );
    assert!(luma_spread(&during, right) > 40.0, "the rest is untouched");

    // The exported video is blurred the same way, from the second second.
    let out = dir.join("out.mp4");
    let opts = ExportOptions::new(ExportFormat::Mp4);
    export(
        &dir,
        &project,
        &out,
        &opts,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    let mut reader = VideoReader::open(&out, None).unwrap();
    let mut spreads = Vec::new();
    while let Some((t, f)) = reader.next_frame().unwrap() {
        spreads.push((t, luma_spread(&f, left)));
    }
    assert!(
        spreads
            .iter()
            .all(|&(t, s)| if t < 1_000_000 { s > 40.0 } else { s < 8.0 }),
        "{spreads:?}"
    );
    cleanup(&dir);
}
