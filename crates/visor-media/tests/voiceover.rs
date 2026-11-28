mod common;

use std::sync::atomic::AtomicBool;

use common::*;
use ffmpeg_next as ffmpeg;
use visor_core::{Project, Track, VoicePiece, Voiceover};
use visor_media::decode::{AudioReader, VideoReader};
use visor_media::{ExportFormat, ExportOptions, export};

const S: u64 = 1_000_000;

/// Peak level of the export's sound in `[from, to)` seconds.
fn peak(path: &std::path::Path, from: f64, to: f64) -> f32 {
    let mut reader = AudioReader::open(path).unwrap();
    let mut peak = 0f32;
    while let Some((at, chunk)) = reader.next_chunk().unwrap() {
        let t = at as f64 / 1e6;
        if t >= from && t < to {
            peak = chunk.iter().fold(peak, |m, s| m.max(s.abs()));
        }
    }
    peak
}

fn exported(dir: &std::path::Path, p: &Project, name: &str) -> std::path::PathBuf {
    let out = dir.join(name);
    export(
        dir,
        p,
        &out,
        &ExportOptions::new(ExportFormat::Mp4),
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    out
}

#[test]
fn a_voiceover_is_heard_over_a_quieter_video_and_its_camera_seen() {
    // 6 s: mic 0.4 and system 0.2, the same tone (0.6 together).
    let (dir, mut p) = project("voiceover", 6);
    // A voiceover over 2–4 s, from 1 s into its files: silence, so only
    // the lowered video is heard there.
    write_tone(&dir.join("voice-1.m4a"), 4 * S, 0.0);
    // Its camera: dark for its first 1.5 s, which this voiceover doesn't
    // use, then bright.
    let cam = dir.join("vcam-1.mp4");
    let mut w = visor_media::VideoWriter::create(&cam, 320, 240, 30).unwrap();
    for i in 0..120u64 {
        let luma = if i < 45 { 16 } else { 235 };
        w.write(&nv12(320, 240, luma, 128), i * FRAME_US, false)
            .unwrap();
    }
    w.finish(4 * S).unwrap();
    p.voiceovers.push(Voiceover {
        pieces: vec![VoicePiece {
            start_us: 2 * S,
            end_us: 4 * S,
            file_us: S,
        }],
        voice: Some("voice-1.m4a".into()),
        camera: Some(Track {
            file: "vcam-1.mp4".into(),
            width: Some(320),
            height: Some(240),
            fps: Some(30),
            start_us: None,
        }),
        clip_sound: 0.25,
    });
    let out = exported(&dir, &p, "ducked.mp4");
    let (outside, under) = (peak(&out, 0.5, 1.5), peak(&out, 2.4, 3.6));
    assert!(outside > 0.5, "{outside}");
    assert!(
        (under - outside * 0.25).abs() < 0.05,
        "{under} vs {outside}"
    );

    // The bubble (bottom left) shows the voiceover's camera there, and the
    // recording's own camera elsewhere.
    let mut reader =
        VideoReader::open(&out, Some((ffmpeg::format::Pixel::YUV420P, 640, 360))).unwrap();
    let mut bubble = |t: u64| {
        reader.seek(t).unwrap();
        let (_, f) = reader.frame_at(t).unwrap().unwrap();
        let (stride, data) = (f.stride(0), f.data(0));
        data[296 * stride + 83]
    };
    let (during, before) = (bubble(3 * S), bubble(S));
    assert!(during > 200, "{during}");
    assert!(before < 200, "{before}");

    // A voice, with the video muted under it: only the voice is heard,
    // from 1 s into its file (loud before that).
    write_tone_with(&dir.join("voice-1.m4a"), 4 * S, |t| {
        if t < S { 0.9 } else { 0.3 }
    });
    p.voiceovers[0].clip_sound = 0.0;
    let out = exported(&dir, &p, "voice.mp4");
    let under = peak(&out, 2.4, 3.6);
    assert!((under - 0.3).abs() < 0.05, "{under}");
    // None of the file before that, right where it starts either.
    let start = peak(&out, 2.16, 2.4);
    assert!(start < 0.4, "{start}");
    assert!(peak(&out, 4.4, 5.5) > 0.5);
    common::cleanup(&dir);
}

#[test]
fn remove_pauses_hears_a_voiceover() {
    // A still screen with no sound of its own, narrated over 4–6 s.
    let (dir, mut p) = project("voiceover-pauses", 10);
    let mut w = visor_media::VideoWriter::create(&dir.join("screen.mp4"), 640, 360, 30).unwrap();
    for i in 0..300u64 {
        w.write(&nv12(640, 360, 100, 128), i * FRAME_US, false)
            .unwrap();
    }
    w.finish(10 * S).unwrap();
    write_tone(&dir.join("mic.m4a"), 10 * S, 0.0);
    write_tone(&dir.join("system.m4a"), 10 * S, 0.0);
    write_tone(&dir.join("voice-1.m4a"), 2 * S, 0.3);
    p.voiceovers.push(Voiceover {
        pieces: vec![VoicePiece {
            start_us: 4 * S,
            end_us: 6 * S,
            file_us: 0,
        }],
        voice: Some("voice-1.m4a".into()),
        camera: None,
        clip_sound: 0.25,
    });
    let quiet = visor_media::dead_time::find(&dir, &p, S, &AtomicBool::new(false)).unwrap();
    assert!(!quiet.is_empty());
    assert!(
        quiet.iter().all(|r| r.1 <= 4 * S || r.0 >= 6 * S),
        "{quiet:?}"
    );
    common::cleanup(&dir);
}
