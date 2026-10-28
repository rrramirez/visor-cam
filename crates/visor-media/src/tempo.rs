//! Streaming time stretch for 48 kHz stereo audio: changes speed without
//! changing pitch (FFmpeg's `atempo`), so sped-up speech still sounds natural.
//! Export also runs its audio cleanup filters through it.

use anyhow::{Context, bail};
use ffmpeg_next::{ChannelLayout, filter, format::Sample, format::sample::Type, frame};

use crate::audio_writer::{CHANNELS, SAMPLE_RATE};

/// Playback speeds the project allows (and `atempo` handles in one stage).
pub const MIN_SPEED: f64 = 0.5;
pub const MAX_SPEED: f64 = 2.0;

pub struct Tempo {
    /// `None` at 1x: samples pass straight through.
    graph: Option<filter::Graph>,
    pts: i64,
}

// SAFETY: the filter graph has no thread affinity; Tempo is never shared.
unsafe impl Send for Tempo {}

impl Tempo {
    pub fn new(speed: f64) -> anyhow::Result<Self> {
        Self::with_filters(speed, &[])
    }

    /// Changes speed, then runs `filters` (FFmpeg filter descriptions, in
    /// order) on the result. Output stays 48 kHz stereo.
    pub fn with_filters(speed: f64, filters: &[&str]) -> anyhow::Result<Self> {
        if !(MIN_SPEED..=MAX_SPEED).contains(&speed) {
            bail!("speed {speed} is outside {MIN_SPEED}..{MAX_SPEED}");
        }
        let mut chain: Vec<String> = Vec::new();
        if (speed - 1.0).abs() >= 1e-6 {
            chain.push(format!("atempo={speed}"));
        }
        chain.extend(filters.iter().map(|f| f.to_string()));
        if chain.is_empty() {
            return Ok(Self {
                graph: None,
                pts: 0,
            });
        }
        // Some filters (loudnorm) change the rate; resample back.
        chain.push(format!(
            "aresample={SAMPLE_RATE},aformat=sample_fmts=flt:sample_rates={SAMPLE_RATE}:channel_layouts=stereo"
        ));
        let mut graph = filter::Graph::new();
        let args = format!(
            "time_base=1/{SAMPLE_RATE}:sample_rate={SAMPLE_RATE}:sample_fmt=flt:channel_layout=stereo"
        );
        graph.add(
            &filter::find("abuffer").context("FFmpeg has no abuffer filter")?,
            "in",
            &args,
        )?;
        graph.add(
            &filter::find("abuffersink").context("FFmpeg has no abuffersink filter")?,
            "out",
            "",
        )?;
        graph
            .output("in", 0)?
            .input("out", 0)?
            .parse(&chain.join(","))
            .with_context(|| format!("FFmpeg can't run the audio filters {chain:?}"))?;
        graph.validate()?;
        Ok(Self {
            graph: Some(graph),
            pts: 0,
        })
    }

    /// Stretches `input` (interleaved stereo) and appends what is ready to
    /// `out`. The filter holds back a little audio until [`flush`](Self::flush).
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) -> anyhow::Result<()> {
        let Some(graph) = &mut self.graph else {
            out.extend_from_slice(input);
            return Ok(());
        };
        let channels = usize::from(CHANNELS);
        let samples = input.len() / channels;
        if samples == 0 {
            return Ok(());
        }
        let mut f = frame::Audio::new(Sample::F32(Type::Packed), samples, ChannelLayout::STEREO);
        f.set_rate(SAMPLE_RATE);
        f.set_pts(Some(self.pts));
        self.pts += samples as i64;
        let bytes = &mut f.data_mut(0)[..samples * channels * 4];
        for (chunk, s) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(input) {
            *chunk = s.to_ne_bytes();
        }
        graph.get("in").context("tempo input")?.source().add(&f)?;
        drain(graph, out)
    }

    /// Ends the stream and appends the rest of the stretched audio.
    pub fn flush(&mut self, out: &mut Vec<f32>) -> anyhow::Result<()> {
        let Some(graph) = &mut self.graph else {
            return Ok(());
        };
        graph.get("in").context("tempo input")?.source().flush()?;
        drain(graph, out)
    }
}

fn drain(graph: &mut filter::Graph, out: &mut Vec<f32>) -> anyhow::Result<()> {
    let mut sink = graph.get("out").context("tempo output")?;
    let mut f = frame::Audio::empty();
    while sink.sink().frame(&mut f).is_ok() {
        let n = f.samples() * usize::from(CHANNELS);
        out.extend(
            f.data(0)[..n * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&b| f32::from_ne_bytes(b)),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stretch(speed: f64, seconds: f64) -> Vec<f32> {
        crate::init().unwrap();
        let mut t = Tempo::new(speed).unwrap();
        let mut out = Vec::new();
        let n = (seconds * f64::from(SAMPLE_RATE)) as usize;
        // 440 Hz tone, fed in 10 ms chunks like export and preview do.
        let tone: Vec<f32> = (0..n)
            .flat_map(|i| {
                let s = (i as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin();
                [s * 0.5, s * 0.5]
            })
            .collect();
        for chunk in tone.chunks(960) {
            t.process(chunk, &mut out).unwrap();
        }
        t.flush(&mut out).unwrap();
        out
    }

    /// Zero crossings per second of the left channel: the pitch.
    fn crossings_per_sec(s: &[f32]) -> f64 {
        let left: Vec<f32> = s.chunks(2).map(|c| c[0]).collect();
        let n = left
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        n as f64 / (left.len() as f64 / f64::from(SAMPLE_RATE))
    }

    #[test]
    fn faster_audio_is_shorter_with_the_same_pitch() {
        for speed in [0.5, 1.0, 1.5, 2.0] {
            let out = stretch(speed, 2.0);
            let secs = out.len() as f64 / 2.0 / f64::from(SAMPLE_RATE);
            assert!(
                (secs - 2.0 / speed).abs() < 0.05,
                "{speed}x gave {secs:.3}s"
            );
            let pitch = crossings_per_sec(&out[out.len() / 4..out.len() * 3 / 4]);
            assert!((pitch - 440.0).abs() < 10.0, "{speed}x pitch {pitch:.1} Hz");
        }
    }

    #[test]
    fn rejects_unsupported_speeds() {
        assert!(Tempo::new(0.25).is_err());
        assert!(Tempo::new(3.0).is_err());
    }
}
