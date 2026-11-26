mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use visor_core::TrackKind;
use visor_media::player::{Player, PreviewFrame};

/// Expected RGB grey for a video-range luma value.
fn grey(y: u8) -> f64 {
    ((f64::from(y) - 16.0) * 255.0 / 219.0).clamp(0.0, 255.0)
}

fn mean_red(f: &PreviewFrame) -> f64 {
    f.rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| f64::from(p[0]))
        .sum::<f64>()
        / f64::from(f.width * f.height)
}

fn wait_frame(p: &mut Player) -> Arc<PreviewFrame> {
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
    common::cleanup(&dir);
}

#[test]
fn plays_into_a_take() {
    let (dir, mut project) = project("player-takes", 2);
    project.tracks.mic = None;
    project.tracks.system = None;
    add_take(&dir, &mut project, 1, (360, 360), false);
    project.takes[0].tracks.mic = None;
    let mut player = Player::open(&dir, &project, (320, 180), 120).unwrap();
    wait_frame(&mut player);
    assert!(player.camera_shown());

    player.seek(2_500_000);
    std::thread::sleep(Duration::from_millis(300));
    let f = wait_frame(&mut player);
    assert_eq!((f.width, f.height), (320, 180));
    // The take's frame 15 in the middle, black bars either side.
    let red_at = |x: u32| {
        let i = ((f.height / 2 * f.width + x) * 4) as usize;
        f64::from(f.rgba[i])
    };
    assert!(
        (red_at(160) - grey(luma(15))).abs() < 12.0,
        "{}",
        red_at(160)
    );
    assert!(red_at(10) < 5.0 && red_at(310) < 5.0);
    // No camera in the take: no bubble.
    assert!(!player.camera_shown());
    common::cleanup(&dir);
}

#[test]
fn builds_thumbnails_and_waveform() {
    // 3 s, then a take of 1 s (with a mic, no system audio).
    let (dir, mut project) = project("preview", 3);
    add_take(&dir, &mut project, 1, (640, 360), true);
    let d = project.duration_us;
    let screen = project.parts(TrackKind::Screen);
    let thumbs = visor_media::preview::thumbnails(&dir, &screen, d, 8, 40).unwrap();
    assert_eq!(thumbs.len(), 8);
    assert!(thumbs.windows(2).all(|w| w[0].0 < w[1].0), "times increase");
    assert!(thumbs[7].0 > 3_000_000, "{}", thumbs[7].0);
    assert_eq!((thumbs[0].1.width, thumbs[0].1.height), (72, 40));

    let audio = [
        project.parts(TrackKind::Mic),
        project.parts(TrackKind::System),
    ];
    let wave = visor_media::preview::waveform(&dir, &audio, d, 40).unwrap();
    assert_eq!(wave.len(), 40);
    assert!(wave.iter().all(|&p| (0.3..0.5).contains(&p)), "{wave:?}");
    common::cleanup(&dir);
}

#[test]
fn plays_at_the_project_speed() {
    let (dir, mut project) = project("player-speed", 4);
    project.tracks.mic = None;
    project.tracks.system = None;
    project.speed = 2.0;

    let mut player = Player::open(&dir, &project, (320, 180), 120).unwrap();
    wait_frame(&mut player);
    player.play();
    std::thread::sleep(Duration::from_millis(500));
    // 0.5 s at 2x covers 1 s of the recording.
    let pos = player.position();
    assert!((850_000..1_250_000).contains(&pos), "position {pos} at 2x");

    // Changing speed mid-play continues from where it is.
    player.set_speed(0.5);
    let at = player.position();
    std::thread::sleep(Duration::from_millis(400));
    let advanced = player.position() - at;
    assert!(
        (120_000..320_000).contains(&advanced),
        "advanced {advanced} at 0.5x"
    );
    common::cleanup(&dir);
}

/// Spread (standard deviation) of red across a preview frame.
fn red_spread(f: &PreviewFrame) -> f64 {
    let reds: Vec<f64> = f
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| f64::from(p[0]))
        .collect();
    let mean = reds.iter().sum::<f64>() / reds.len() as f64;
    (reds.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / reds.len() as f64).sqrt()
}

#[test]
fn preview_shows_blurs_and_their_changes() {
    let (dir, mut project) = checkerboard_project("player-blur", 640, 360, 2);
    let whole = visor_core::Blur {
        start_us: 0,
        end_us: project.duration_us,
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
    };
    project.blurs = vec![whole.clone()];
    let mut player = Player::open(&dir, &project, (640, 360), 120).unwrap();
    let blurred = wait_frame(&mut player);
    assert!(red_spread(&blurred) < 10.0, "{}", red_spread(&blurred));

    // The same frame, redrawn without the blur, then with it again.
    player.set_blurs(Vec::new());
    let sharp = wait_frame(&mut player);
    assert!(red_spread(&sharp) > 50.0, "{}", red_spread(&sharp));
    player.set_blurs(vec![whole]);
    assert!(red_spread(&wait_frame(&mut player)) < 10.0);
    cleanup(&dir);
}
