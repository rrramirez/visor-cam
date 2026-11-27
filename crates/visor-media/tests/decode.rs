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

fn part(start: u64, end: u64, file: Option<&str>) -> visor_core::Part {
    visor_core::Part {
        start,
        end,
        file: file.map(String::from),
        offset: 0,
    }
}

/// Mean luma of a frame's first plane over columns `[x0, x1)`.
fn luma_between(f: &ffmpeg_next::frame::Video, x0: usize, x1: usize) -> f64 {
    let (h, stride) = (f.height() as usize, f.stride(0));
    let data = f.data(0);
    let sum: u64 = (0..h)
        .flat_map(|y| data[y * stride + x0..y * stride + x1].iter())
        .map(|&v| u64::from(v))
        .sum();
    sum as f64 / ((x1 - x0) * h) as f64
}

#[test]
fn reads_takes_as_one_track_fitting_each_into_the_frame() {
    use ffmpeg_next::format::Pixel;
    let dir = temp_dir("parts");
    write_video(&dir.join("screen.mp4"), 640, 360, FPS, 128);
    // A square second take, and none of the track in between.
    write_video(&dir.join("screen-3.mp4"), 360, 360, FPS, 128);
    let parts = [
        part(0, 1_000_000, Some("screen.mp4")),
        part(1_000_000, 2_000_000, None),
        part(2_000_000, 3_000_000, Some("screen-3.mp4")),
    ];
    let mut reader =
        VideoReader::open_parts(&dir, &parts, Some((Pixel::YUV420P, 320, 180))).unwrap();
    assert_eq!(reader.size(), (640, 360));
    let mut times = Vec::new();
    while let Some((pts, f)) = reader.next_frame().unwrap() {
        assert_eq!((f.width(), f.height()), (320, 180));
        times.push(pts);
    }
    assert_eq!(times.len(), 2 * FPS as usize);
    assert!(times.windows(2).all(|w| w[0] < w[1]), "{times:?}");
    assert_eq!(times[FPS as usize - 1], (FPS - 1) * FRAME_US);
    assert_eq!(times[FPS as usize], 2_000_000);
    assert!(reader.shows(500_000) && !reader.shows(1_500_000) && reader.shows(2_000_000));

    // Seeking lands in the take, and its frames keep their shape.
    let at = 2_000_000 + 15 * FRAME_US;
    reader.seek(at).unwrap();
    let (pts, f) = reader.frame_at(at).unwrap().unwrap();
    assert_eq!(pts, at);
    let middle = luma_between(&f, 140, 180);
    assert!((middle - f64::from(luma(15))).abs() < 6.0, "{middle}");
    // Black bars either side (video black is 16).
    assert!(luma_between(&f, 0, 60) < 20.0 && luma_between(&f, 260, 320) < 20.0);

    // In the gap, the next take.
    reader.seek(1_500_000).unwrap();
    assert_eq!(reader.next_frame().unwrap().unwrap().0, 2_000_000);
    common::cleanup(&dir);
}

#[test]
fn reads_audio_takes_in_their_place() {
    let dir = temp_dir("aparts");
    write_tone(&dir.join("mic.m4a"), 1_000_000, 0.4);
    write_tone(&dir.join("mic-3.m4a"), 1_000_000, 0.4);
    let parts = [
        part(0, 1_000_000, Some("mic.m4a")),
        part(1_000_000, 2_000_000, None),
        part(2_000_000, 3_000_000, Some("mic-3.m4a")),
    ];
    let mut reader = AudioReader::open_parts(&dir, &parts).unwrap();
    let mut chunks = Vec::new();
    while let Some((at, chunk)) = reader.next_chunk().unwrap() {
        chunks.push((at, chunk.len() / 2));
    }
    assert!(
        chunks
            .iter()
            .all(|&(at, _)| !(1_000_000..2_000_000).contains(&at))
    );
    let second: usize = chunks
        .iter()
        .filter(|c| c.0 >= 2_000_000)
        .map(|c| c.1)
        .sum();
    assert!((second as i64 - 48_000).abs() < 2_048, "{second} samples");
    reader.seek(2_500_000).unwrap();
    let (at, _) = reader.next_chunk().unwrap().unwrap();
    assert!((2_400_000..=2_500_000).contains(&at), "seek landed at {at}");
    common::cleanup(&dir);
}

#[test]
fn reads_a_phone_video_upright_from_its_first_frame() {
    use ffmpeg_next::format::Pixel;
    let dir = temp_dir("phone");
    write_video(&dir.join("screen.mp4"), 640, 360, FPS, 128);
    let (video, tone) = (dir.join("v.mp4"), dir.join("t.m4a"));
    write_halves(&video, 320, 180, 2 * FPS);
    write_tone(&tone, 2_000_000, 0.4);
    // Stored sideways, shown turned clockwise, starting 1.4 s in.
    remux(
        &[&video, &tone],
        &dir.join("screen-2.mov"),
        1_400_000,
        Some(90.0),
    );
    let phone = visor_core::Part {
        offset: 1_400_000,
        ..part(1_000_000, 3_000_000, Some("screen-2.mov"))
    };

    let parts = [phone.clone()];
    let reader = VideoReader::open_parts(&dir, &parts, None).unwrap();
    assert_eq!(reader.size(), (180, 320));
    let parts = [part(0, 1_000_000, Some("screen.mp4")), phone.clone()];
    let mut reader =
        VideoReader::open_parts(&dir, &parts, Some((Pixel::YUV420P, 180, 320))).unwrap();
    reader.seek(1_000_000).unwrap();
    let (pts, f) = reader.frame_at(1_000_000).unwrap().unwrap();
    assert_eq!(pts, 1_000_000);
    // Its left half, bright, is on top once turned.
    let (stride, data) = (f.stride(0), f.data(0));
    let rows = |y0: usize, y1: usize| {
        let sum: u64 = (y0..y1)
            .flat_map(|y| data[y * stride..y * stride + 180].iter())
            .map(|&v| u64::from(v))
            .sum();
        sum as f64 / ((y1 - y0) * 180) as f64
    };
    assert!(
        rows(0, 150) > 200.0 && rows(170, 320) < 50.0,
        "{} {}",
        rows(0, 150),
        rows(170, 320)
    );
    // The last frame is at its end, not 1.4 s past it.
    let mut last = 0;
    while let Some((pts, _)) = reader.next_frame().unwrap() {
        last = pts;
    }
    assert!(last.abs_diff(3_000_000 - FRAME_US) < 2 * FRAME_US, "{last}");

    // Its sound plays in its place too, sharing the file.
    let mut audio = AudioReader::open_parts(&dir, &[phone]).unwrap();
    let mut first = None;
    let mut samples = 0;
    while let Some((at, chunk)) = audio.next_chunk().unwrap() {
        first.get_or_insert(at);
        samples += chunk.len() / 2;
    }
    assert!(
        first.is_some_and(|f| f.abs_diff(1_000_000) < 30_000),
        "{first:?}"
    );
    assert!((samples as i64 - 96_000).abs() < 2_048, "{samples} samples");
    common::cleanup(&dir);
}
