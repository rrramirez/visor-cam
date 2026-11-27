mod common;

use std::sync::atomic::{AtomicBool, Ordering};

use common::*;
use visor_media::import::{copy_in, probe_video};

#[test]
fn probes_a_video_from_elsewhere() {
    let dir = temp_dir("probe");
    let (video, tone) = (dir.join("v.mp4"), dir.join("t.m4a"));
    write_halves(&video, 320, 180, 2 * FPS);
    write_tone(&tone, 2_000_000, 0.4);
    // From a phone held upright: stored sideways, shown turned clockwise,
    // and starting 1.4 s in.
    let phone = dir.join("IMG_0001.MOV");
    remux(&[&video, &tone], &phone, 1_400_000, Some(-90.0));
    let info = probe_video(&phone).unwrap();
    assert_eq!(info.size, (180, 320));
    assert!(info.has_audio);
    assert!(
        info.start.abs_diff(1_400_000) < 50_000,
        "start {}",
        info.start
    );
    assert!(
        info.duration.abs_diff(2_000_000) < 100_000,
        "duration {}",
        info.duration
    );

    // No sound: silent.
    let quiet = dir.join("quiet.mkv");
    remux(&[&video], &quiet, 0, None);
    let info = probe_video(&quiet).unwrap();
    assert_eq!(
        (info.size, info.has_audio, info.start),
        ((320, 180), false, 0)
    );

    // Not a video it can add.
    let sound = dir.join("sound.mp4");
    remux(&[&tone], &sound, 0, None);
    let refused = |path: &std::path::Path| probe_video(path).unwrap_err().to_string();
    assert!(refused(&sound).contains("no video"), "{}", refused(&sound));
    let avi = dir.join("clip.avi");
    std::fs::copy(&video, &avi).unwrap();
    assert!(refused(&avi).contains("MP4"), "{}", refused(&avi));
    let text = dir.join("notes.mp4");
    std::fs::write(&text, "not a video").unwrap();
    assert!(probe_video(&text).is_err());
    common::cleanup(&dir);
}

#[test]
fn copies_a_file_in_with_progress_and_can_be_cancelled() {
    let dir = temp_dir("copy-in");
    let src = dir.join("big.mp4");
    let data: Vec<u8> = (0..5_000_000u32).map(|i| (i * 31 % 251) as u8).collect();
    std::fs::write(&src, &data).unwrap();

    let dst = dir.join("screen-2.mp4");
    let mut seen = Vec::new();
    copy_in(&src, &dst, &mut |p| seen.push(p), &AtomicBool::new(false)).unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), data);
    assert!(seen.len() > 1 && seen.windows(2).all(|w| w[0] <= w[1]));
    assert_eq!(seen.last().copied(), Some(1.0));

    // Cancelled: nothing left behind.
    let cancel = AtomicBool::new(false);
    let dst = dir.join("screen-3.mp4");
    let err = copy_in(
        &src,
        &dst,
        &mut |_| cancel.store(true, Ordering::Relaxed),
        &cancel,
    );
    assert!(err.is_err());
    let left: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(left.len(), 2, "{left:?}");
    common::cleanup(&dir);
}
