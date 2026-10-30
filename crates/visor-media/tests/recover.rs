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
    truncate(&dir.join("system.m4a"), 0.6);
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
fn crash_during_a_still_screen_keeps_the_narration() {
    // A second of screen changes, then a still screen (capture delivers no
    // frames) while the mic records two more seconds, then a crash: the
    // writers are never finished, so whatever they hold in memory is lost.
    let dir = common::temp_dir("recover-still");
    let mut video = VideoWriter::create(&dir.join("screen.mp4"), 640, 360, 30).unwrap();
    for i in 0..30 {
        let frame = common::nv12(640, 360, common::luma(i), 128);
        video.write(&frame, i * common::FRAME_US, false).unwrap();
    }
    // The recorder flushes a track that has had nothing to write for a
    // while; the hardware encoder has long caught up by then.
    std::thread::sleep(std::time::Duration::from_millis(300));
    video.flush().unwrap();
    let mut mic = AudioWriter::create(&dir.join("mic.m4a")).unwrap();
    for i in 0..300 {
        let buf = AudioBuffer {
            host_ts: 0,
            sample_rate: 48_000,
            channels: 2,
            samples: vec![0.1; 960],
        };
        mic.write(&buf, i * 10_000).unwrap();
    }
    std::mem::forget(video);
    std::mem::forget(mic);

    let project = recover::recover(&dir).unwrap();
    let secs = project.duration_us as f64 / 1e6;
    // Audio fragments end every half second: at most that is lost.
    assert!(secs >= 2.4, "recovered {secs:.2}s of 3s");
    assert!(project.tracks.mic.is_some());
    // The frames were flushed, all but the few a hardware encoder holds
    // back until more come (about 0.2 s on a Mac), and the last one holds
    // to the end.
    let mut reader = VideoReader::open(&dir.join("screen.mp4"), None).unwrap();
    let mut frames = 0;
    while reader.next_frame().unwrap().is_some() {
        frames += 1;
    }
    assert!(frames >= 20, "{frames} of 30 frames");
    let end = visor_media::export::frame_jpeg(&dir, &project, project.duration_us - 1, 360);
    assert!(end.is_ok_and(|jpeg| !jpeg.is_empty()));
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
