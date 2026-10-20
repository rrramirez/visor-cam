//! Synthetic recordings for tests: frame `i` has uniform luma `luma(i)`, and
//! audio is a 440 Hz tone.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use visor_core::{AudioBuffer, PixelFormat, Plane, Project, Track, VideoFrame};
use visor_media::{AudioWriter, VideoWriter};

pub const FPS: u64 = 30;
pub const FRAME_US: u64 = 1_000_000 / FPS;

pub fn luma(i: u64) -> u8 {
    (16 + (i * 7) % 200) as u8
}

pub fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("visor-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn nv12(w: u32, h: u32, y: u8, uv: u8) -> VideoFrame {
    let (w, h) = (w as usize, h as usize);
    VideoFrame {
        host_ts: 0,
        width: w as u32,
        height: h as u32,
        format: PixelFormat::Nv12,
        planes: vec![
            Plane {
                data: vec![y; w * h],
                stride: w,
            },
            Plane {
                data: vec![uv; w * h / 2],
                stride: w,
            },
        ],
    }
}

pub fn write_video(path: &Path, w: u32, h: u32, frames: u64, uv: u8) {
    let mut writer = VideoWriter::create(path, w, h, FPS as u32).unwrap();
    for i in 0..frames {
        writer
            .write(&nv12(w, h, luma(i), uv), i * FRAME_US, false)
            .unwrap();
    }
    writer.finish(frames * FRAME_US).unwrap();
}

pub fn write_tone(path: &Path, duration_us: u64, amplitude: f32) {
    let mut writer = AudioWriter::create(path).unwrap();
    let chunk = 480; // 10 ms at 48 kHz
    let total = duration_us * 48 / 1000;
    let mut n = 0u64;
    while n < total {
        let samples: Vec<f32> = (n..n + chunk)
            .flat_map(|i| {
                let s = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * amplitude;
                [s, s]
            })
            .collect();
        let buf = AudioBuffer {
            host_ts: 0,
            sample_rate: 48_000,
            channels: 2,
            samples,
        };
        writer.write(&buf, n * 1000 / 48).unwrap();
        n += chunk;
    }
    writer.finish(duration_us).unwrap();
}

/// A project folder with screen (640x360), camera (320x240), mic and system tracks.
pub fn project(name: &str, seconds: u64) -> (PathBuf, Project) {
    let dir = temp_dir(name);
    let frames = seconds * FPS;
    let duration = frames * FRAME_US;
    write_video(&dir.join("screen.mp4"), 640, 360, frames, 128);
    write_video(&dir.join("camera.mp4"), 320, 240, frames, 200);
    write_tone(&dir.join("mic.m4a"), duration, 0.4);
    write_tone(&dir.join("system.m4a"), duration, 0.2);
    let mut p = Project::new(name, "2026-10-20T00:00:00Z", duration);
    let track = |file: &str, w, h| Track {
        file: file.into(),
        width: w,
        height: h,
        fps: Some(30),
    };
    p.tracks.screen = Some(track("screen.mp4", Some(640), Some(360)));
    p.tracks.camera = Some(track("camera.mp4", Some(320), Some(240)));
    p.tracks.mic = Some(track("mic.m4a", None, None));
    p.tracks.system = Some(track("system.m4a", None, None));
    p.save(&dir).unwrap();
    (dir, p)
}

/// Mean luma of a decoded frame's first plane.
pub fn mean_luma(f: &ffmpeg_next::frame::Video) -> f64 {
    let (w, h, stride) = (f.width() as usize, f.height() as usize, f.stride(0));
    let data = f.data(0);
    let sum: u64 = (0..h)
        .flat_map(|y| data[y * stride..y * stride + w].iter())
        .map(|&v| u64::from(v))
        .sum();
    sum as f64 / (w * h) as f64
}
