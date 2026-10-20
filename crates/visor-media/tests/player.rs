mod common;

use std::time::{Duration, Instant};

use common::*;
use visor_media::player::{Player, PreviewFrame};

/// Expected RGB grey for a video-range luma value.
fn grey(y: u8) -> f64 {
    ((f64::from(y) - 16.0) * 255.0 / 219.0).clamp(0.0, 255.0)
}

fn mean_red(f: &PreviewFrame) -> f64 {
    f.rgba.chunks_exact(4).map(|p| f64::from(p[0])).sum::<f64>() / f64::from(f.width * f.height)
}

fn wait_frame(p: &mut Player) -> PreviewFrame {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some((Some(screen), _)) = p.take_frames() {
            return screen;
        }
        assert!(Instant::now() < deadline, "no frame published");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn scrubs_plays_through_cuts_and_stops_at_end() {
    let (dir, mut project) = project("player", 4);
    // No audio: keep tests silent.
    project.tracks.mic = None;
    project.tracks.system = None;
    project.edit.cut(1_000_000, 2_000_000);

    let mut player = Player::open(&dir, &project, (320, 180), 120).unwrap();
    let first = wait_frame(&mut player);
    assert_eq!((first.width, first.height), (320, 180));

    // Scrub to 2.5 s (frame 75).
    player.seek(2_500_000);
    std::thread::sleep(Duration::from_millis(300));
    let f = wait_frame(&mut player);
    assert!(
        (mean_red(&f) - grey(luma(75))).abs() < 10.0,
        "{} vs {}",
        mean_red(&f),
        grey(luma(75))
    );

    // Seeking into the cut lands at its end.
    player.seek(1_500_000);
    assert_eq!(player.position(), 2_000_000);

    // Play from 0.8 s: after ~0.5 s of playback we're past the cut.
    player.seek(800_000);
    player.play();
    std::thread::sleep(Duration::from_millis(500));
    let pos = player.position();
    assert!((2_100_000..2_600_000).contains(&pos), "position {pos}");

    // Plays to the end (3 s of output) and stops.
    std::thread::sleep(Duration::from_millis(2_300));
    assert!(!player.is_playing());
    assert!(
        player.position() >= 3_900_000,
        "end position {}",
        player.position()
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn builds_thumbnails_and_waveform() {
    let (dir, project) = project("preview", 3);
    let thumbs =
        visor_media::preview::thumbnails(&dir.join("screen.mp4"), project.duration_us, 6, 40)
            .unwrap();
    assert_eq!(thumbs.len(), 6);
    assert!(thumbs.windows(2).all(|w| w[0].0 < w[1].0), "times increase");
    assert_eq!((thumbs[0].1.width, thumbs[0].1.height), (72, 40));

    let mic = dir.join("mic.m4a");
    let system = dir.join("system.m4a");
    let wave = visor_media::preview::waveform(&[&mic, &system], project.duration_us, 30).unwrap();
    assert_eq!(wave.len(), 30);
    assert!(wave.iter().all(|&p| (0.3..0.5).contains(&p)), "{wave:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}
