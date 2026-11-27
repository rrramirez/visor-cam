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
    remux(&[&video, &tone], &phone, 1_400_000, Some(90.0));
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

#[test]
fn adds_a_recording_and_a_phone_video_at_the_end() {
    use visor_core::{Blur, Marker, Range, Tracks};
    use visor_media::import::prepare;
    use visor_media::{ExportFormat, ExportOptions, export};
    const S: u64 = 1_000_000;
    let (dir, mut project) = project("add-into", 2);
    // Another recording, 3 s, cut 0.5–1 s, split at 2 s with its last
    // clip played first, a marker and a blur; then recorded more onto.
    let (other, mut theirs) = common::project("add-other", 3);
    theirs.edit.cut(S / 2, S);
    theirs.edit.split(2 * S);
    theirs.edit.move_clip(2, 0);
    theirs.markers = vec![Marker {
        at_us: 2 * S + S / 2,
        text: "Theirs".into(),
    }];
    theirs.blurs = vec![Blur {
        start_us: 0,
        end_us: 3 * S,
        x: 0.0,
        y: 0.0,
        width: 0.5,
        height: 0.5,
    }];
    add_take(&other, &mut theirs, 1, (640, 360), false);
    // A phone's video, held upright, starting 1.4 s in.
    let elsewhere = temp_dir("add-phone");
    let (video, tone) = (elsewhere.join("v.mp4"), elsewhere.join("t.m4a"));
    write_halves(&video, 320, 180, 2 * FPS);
    write_tone(&tone, 2 * S, 0.4);
    let phone = elsewhere.join("IMG_0001.MOV");
    remux(&[&video, &tone], &phone, 1_400_000, Some(90.0));

    let prepared = prepare(&dir, &project, &[other.clone(), phone.clone()]).unwrap();
    prepared
        .copy(&dir, &mut |_| {}, &AtomicBool::new(false))
        .unwrap();
    let starts: Vec<u64> = prepared
        .additions
        .into_iter()
        .map(|a| project.add(a))
        .collect();
    // To the nearest 10 ms (the test recordings' frames are 33 333 µs).
    let cs = |t: u64| (t + 5_000) / 10_000 * 10;
    assert_eq!(
        starts.iter().map(|&t| cs(t)).collect::<Vec<_>>(),
        [2000, 6000]
    );
    // The phone's video lasts as long as its sound, a little over 2 s.
    assert!(
        (8000..8100).contains(&cs(project.duration_us)),
        "{}",
        project.duration_us
    );
    let ms = |r: &Range| (cs(r.0), cs(r.1));
    assert_eq!(
        project.edit.ranges().iter().map(ms).collect::<Vec<_>>(),
        [
            (0, 2000),
            (4000, 5000),
            (2000, 2500),
            (3000, 4000),
            (5000, 6000),
            (6000, cs(project.duration_us))
        ]
    );
    assert_eq!(project.takes.len(), 3);
    let at = starts[0];
    assert_eq!(
        project.markers.last().map(|m| m.at_us),
        Some(at + 2 * S + S / 2)
    );
    let b = &project.blurs[0];
    // Stretched over the take recorded onto it, as there.
    assert_eq!((b.start_us, b.end_us), (at, at + theirs.duration_us));
    // The phone's video and sound are one file.
    let phone_take: &Tracks = &project.takes[2].tracks;
    let (screen, mic) = (
        phone_take.screen.as_ref().unwrap(),
        phone_take.mic.as_ref().unwrap(),
    );
    assert_eq!(screen.file, mic.file);
    assert!(screen.file.ends_with(".MOV") || screen.file.ends_with(".mov"));
    assert_eq!((screen.width, screen.height), (Some(180), Some(320)));
    for take in &project.takes {
        for track in [
            &take.tracks.screen,
            &take.tracks.camera,
            &take.tracks.mic,
            &take.tracks.system,
        ]
        .into_iter()
        .flatten()
        {
            assert!(dir.join(&track.file).is_file(), "{}", track.file);
        }
    }
    project.save(&dir).unwrap();
    assert_eq!(visor_core::Project::load(&dir).unwrap(), project);

    let out = dir.join("out.mp4");
    export(
        &dir,
        &project,
        &out,
        &ExportOptions::new(ExportFormat::Mp4),
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    let mut reader = visor_media::decode::VideoReader::open(
        &out,
        Some((ffmpeg_next::format::Pixel::YUV420P, 640, 360)),
    )
    .unwrap();
    assert!(
        reader.duration().abs_diff(7 * S + S / 2) < 100_000,
        "{}",
        reader.duration()
    );
    // 1 s into the phone's video: upright, its bright half on top, with
    // black bars either side.
    reader.seek(6 * S + S / 2).unwrap();
    let (_, f) = reader.frame_at(6 * S + S / 2).unwrap().unwrap();
    let (stride, data) = (f.stride(0), f.data(0));
    let at = |x: usize, y: usize| data[y * stride + x];
    assert!(
        at(320, 60) > 180 && at(320, 300) < 60,
        "{} {}",
        at(320, 60),
        at(320, 300)
    );
    assert!(at(40, 60) < 25 && at(600, 60) < 25);

    // Refused: itself, and a recording still being recorded onto.
    assert!(prepare(&dir, &project, std::slice::from_ref(&dir)).is_err());
    theirs.begin_take(Tracks::default());
    theirs.save(&other).unwrap();
    let err = format!(
        "{:#}",
        prepare(&dir, &project, std::slice::from_ref(&other)).unwrap_err()
    );
    assert!(err.contains("being recorded"), "{err}");
    for d in [dir, other, elsewhere] {
        common::cleanup(&d);
    }
}

#[test]
fn preparing_keeps_the_take_numbers_it_picks() {
    use visor_media::import::prepare;
    let (dir, project) = project("add-reserve", 1);
    let elsewhere = temp_dir("add-reserve-src");
    let video = elsewhere.join("clip.mp4");
    write_video(&video, 320, 180, FPS, 128);
    let prepared = prepare(&dir, &project, std::slice::from_ref(&video)).unwrap();
    let name = prepared.files().next().unwrap().to_string();
    assert_eq!(name, "screen-2.mp4");
    // Taken while it's copied: a take recorded meanwhile gets another.
    assert_eq!(project.free_take_number(&dir), 3);
    prepared
        .copy(&dir, &mut |_| {}, &AtomicBool::new(false))
        .unwrap();
    assert!(dir.join(&name).metadata().unwrap().len() > 0);

    // A take recorded after a gap (an addition undone, its file deleted):
    // each one added skips it.
    std::fs::write(dir.join("screen-4.mp4"), b"take").unwrap();
    let two = prepare(&dir, &project, &[video.clone(), video.clone()]).unwrap();
    assert_eq!(
        two.files().collect::<Vec<_>>(),
        ["screen-3.mp4", "screen-5.mp4"]
    );
    two.copy(&dir, &mut |_| {}, &AtomicBool::new(false))
        .unwrap();

    // One it can't add: nothing is kept for any of them.
    let bad = elsewhere.join("bad.mp4");
    std::fs::write(&bad, "not a video").unwrap();
    let before: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(prepare(&dir, &project, &[video.clone(), bad]).is_err());
    let after: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(before.len(), after.len());
    common::cleanup(&dir);
    common::cleanup(&elsewhere);
}

#[test]
fn pickers_list_upper_case_names_too() {
    // Linux pickers match extensions case by case; phones name files IMG_0001.MOV.
    let exts = visor_media::import::picker_extensions();
    assert!(exts.iter().any(|e| e == "mov") && exts.iter().any(|e| e == "MOV"));
    assert!(visor_media::import::is_video_file(std::path::Path::new(
        "IMG_0001.MOV"
    )));
}

#[test]
fn finds_what_an_interrupted_addition_left() {
    let (dir, project) = project("add-left", 1);
    std::fs::write(dir.join("screen-2.partial"), b"half").unwrap();
    std::fs::write(dir.join("screen-2.mov"), b"").unwrap();
    // Empty, but the project's: kept. Not empty: kept.
    std::fs::write(dir.join("notes.txt"), b"mine").unwrap();
    let mut left = visor_media::import::leftovers(&dir, &project);
    left.sort();
    assert_eq!(left, ["screen-2.mov", "screen-2.partial"]);
    common::cleanup(&dir);
}
