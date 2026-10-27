mod common;

use std::time::Instant;

use common::*;
use visor_media::decode::{AudioReader, VideoReader};

#[test]
fn seeks_frame_accurately_in_fragmented_mp4() {
    let dir = temp_dir("seek");
    let path = dir.join("screen.mp4");
    write_video(&path, 640, 360, 30 * FPS, 128);

    let mut reader = VideoReader::open(&path, None).unwrap();
    assert!(
        (reader.duration() as i64 - 30_000_000).abs() < 100_000,
        "{}",
        reader.duration()
    );
    for target_frame in [0u64, 17 * 30 + 15, 29 * 30 + 29, 5 * 30] {
        let at = target_frame * FRAME_US;
        let start = Instant::now();
        reader.seek(at).unwrap();
        let (pts, frame) = reader.frame_at(at).unwrap().unwrap();
        let took = start.elapsed();
        assert_eq!(pts / FRAME_US, target_frame, "pts {pts}");
        let expected = f64::from(luma(target_frame));
        assert!(
            (mean_luma(&frame) - expected).abs() < 6.0,
            "luma {} vs {expected}",
            mean_luma(&frame)
        );
        assert!(took.as_millis() < 1500, "seek took {took:?}");
    }
    common::cleanup(&dir);
}

#[test]
fn reads_audio_as_48k_stereo() {
    let dir = temp_dir("areader");
    let path = dir.join("mic.m4a");
    write_tone(&path, 2_000_000, 0.4);
    let mut reader = AudioReader::open(&path).unwrap();
    let mut total = 0;
    let mut peak = 0f32;
    while let Some((_, chunk)) = reader.next_chunk().unwrap() {
        total += chunk.len() / 2;
        peak = chunk.iter().fold(peak, |m, s| m.max(s.abs()));
    }
    assert!((total as i64 - 96_000).abs() < 2_048, "{total} samples");
    assert!((0.3..0.5).contains(&peak), "peak {peak}");
    reader.seek(1_000_000).unwrap();
    let (at, _) = reader.next_chunk().unwrap().unwrap();
    assert!(at <= 1_000_000 && at > 900_000, "seek landed at {at}");
    common::cleanup(&dir);
}

#[test]
fn refuses_file_lists_posing_as_tracks() {
    let dir = temp_dir("playlist");
    let real = dir.join("real.mp4");
    write_video(&real, 64, 64, FPS, 128);
    let fake = dir.join("screen.mp4");
    // The concat demuxer reads other files named in it.
    std::fs::write(&fake, "ffconcat version 1.0\nfile real.mp4\n").unwrap();
    assert!(VideoReader::open(&real, None).is_ok());
    assert!(VideoReader::open(&fake, None).is_err());
    common::cleanup(&dir);
}
