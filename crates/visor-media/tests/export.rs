mod common;

use std::sync::atomic::AtomicBool;

use common::*;
use ffmpeg_next as ffmpeg;
use visor_media::decode::VideoReader;
use visor_media::{ExportFormat, ExportOptions, export};

fn probe(path: &std::path::Path) -> (f64, Vec<ffmpeg::codec::Id>) {
    let input = ffmpeg::format::input(path).unwrap();
    let duration = input.duration() as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE);
    let codecs = input.streams().map(|s| s.parameters().id()).collect();
    (duration, codecs)
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
    assert_eq!(codecs, [ffmpeg::codec::Id::H264, ffmpeg::codec::Id::AAC]);

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
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn exports_webm_vp9_and_av1() {
    let (dir, mut project) = project("export-webm", 3);
    project.edit.trim(500_000, 2_500_000);
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
    }
    std::fs::remove_dir_all(&dir).unwrap();
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
    std::fs::remove_dir_all(&dir).unwrap();
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
    std::fs::remove_dir_all(&dir).unwrap();
}
