mod common;

use std::fs::OpenOptions;

use visor_core::AudioBuffer;
use visor_core::project::PROJECT_FILE;
use visor_media::decode::VideoReader;
use visor_media::recover;
use visor_media::{AudioWriter, VideoWriter};

/// Cuts a file to `fraction` of its size, as a crash mid-write would.
fn truncate(path: &std::path::Path, fraction: f64) {
    let len = std::fs::metadata(path).unwrap().len();
    let f = OpenOptions::new().write(true).open(path).unwrap();
    f.set_len((len as f64 * fraction) as u64).unwrap();
}

#[test]
fn crashed_recording_is_recovered_from_its_tracks() {
    let (dir, _) = common::project("recover-crash", 6);
    // A crash: no project file, tracks cut off mid-fragment, and a mic track
    // that never got past its header.
    std::fs::remove_file(dir.join(PROJECT_FILE)).unwrap();
    truncate(&dir.join("screen.mp4"), 0.6);
    truncate(&dir.join("camera.mp4"), 0.6);
    truncate(&dir.join("mic.m4a"), 0.0);
    assert!(recover::is_interrupted(&dir));

    let project = recover::load_or_recover(&dir).unwrap();
    assert!(!recover::is_interrupted(&dir), "project.json was written");
    let secs = project.duration_us as f64 / 1e6;
    assert!((1.5..5.9).contains(&secs), "recovered {secs:.2}s of 6s");
    assert_eq!(project.edit.duration(), project.duration_us);
    let screen = project.tracks.screen.as_ref().unwrap();
    assert_eq!((screen.width, screen.height), (Some(640), Some(360)));
    assert!(project.tracks.camera.is_some());
    assert!(project.tracks.mic.is_none(), "unreadable mic track dropped");
    assert!(project.tracks.system.is_some());

    // The recovered screen track decodes.
    let mut reader = VideoReader::open(&dir.join("screen.mp4"), None).unwrap();
    assert!(reader.next_frame().unwrap().is_some());
}

#[test]
fn short_crashed_recording_keeps_all_but_its_last_moments() {
    // A crash a second and a half in, on a screen that barely changes: the
    // writers are never finished, and whatever they hold in memory is lost.
    let dir = common::temp_dir("recover-short");
    let frames = 45;
    let mut video = VideoWriter::create(&dir.join("screen.mp4"), 640, 360, 30).unwrap();
    for i in 0..frames {
        let y = 100 + (i % 2) as u8;
        video
            .write(&common::nv12(640, 360, y, 128), i * common::FRAME_US, false)
            .unwrap();
    }
    let mut audio = AudioWriter::create(&dir.join("mic.m4a")).unwrap();
    for i in 0..150 {
        let buf = AudioBuffer {
            host_ts: 0,
            sample_rate: 48_000,
            channels: 2,
            samples: vec![0.1; 960],
        };
        audio.write(&buf, i * 10_000).unwrap();
    }
    std::mem::forget(video);
    std::mem::forget(audio);

    let project = recover::recover(&dir).unwrap();
    let secs = project.duration_us as f64 / 1e6;
    assert!(secs >= 0.9, "recovered {secs:.2}s of 1.5s");
    assert!(project.tracks.mic.is_some(), "mic track recovered");
    common::cleanup(&dir);
}

#[test]
fn finished_recordings_are_left_alone() {
    let (dir, original) = common::project("recover-finished", 2);
    assert!(!recover::is_interrupted(&dir));
    assert_eq!(recover::load_or_recover(&dir).unwrap(), original);
}

#[test]
fn project_written_at_start_without_duration_is_recovered() {
    let (made, mut p) = common::project("recover-started", 3);
    // Its own save folder, so recover_all doesn't touch other tests' folders.
    let root = common::temp_dir("recover-started-root");
    let dir = root.join("recording");
    std::fs::rename(&made, &dir).unwrap();
    p.duration_us = 0;
    p.edit = visor_core::EditList::full(0);
    p.save(&dir).unwrap();
    assert!(recover::is_interrupted(&dir));
    let recovered = recover::recover_all(&root);
    assert!(recovered.contains(&dir));
    let p = visor_core::Project::load(&dir).unwrap();
    assert!(
        (p.duration_us as f64 / 1e6 - 3.0).abs() < 0.2,
        "{}",
        p.duration_us
    );
}
