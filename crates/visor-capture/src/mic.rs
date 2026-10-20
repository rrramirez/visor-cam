//! Microphone capture via cpal (CoreAudio / WASAPI / ALSA-PulseAudio-PipeWire).
//!
//! A [`MicStream`] runs for as long as it is alive, so the recorder panel can
//! show a level meter before recording, and the recorder subscribes to the
//! same stream when recording starts.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;

use anyhow::{Context, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Data, InputCallbackInfo, SampleFormat};
use crossbeam_channel::{Receiver, Sender, bounded};
use visor_core::{AudioBuffer, Micros};

use crate::{Tap, host_now};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicDevice {
    /// Stable id to pass to [`MicStream::open`].
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

pub fn list() -> Vec<MicDevice> {
    let host = cpal::default_host();
    let default_id = host
        .default_input_device()
        .and_then(|d| d.id().ok())
        .map(|id| id.to_string());
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    devices
        .filter_map(|d| {
            let id = d.id().ok()?.to_string();
            let name = d
                .description()
                .map(|desc| desc.name().to_owned())
                .unwrap_or_else(|_| id.clone());
            Some(MicDevice {
                is_default: Some(&id) == default_id.as_ref(),
                id,
                name,
            })
        })
        .collect()
}

pub struct MicStream {
    tap: Tap<Arc<AudioBuffer>>,
    /// Peak level (0.0–1.0) since the last [`MicStream::level`] call, as f32 bits.
    peak: Arc<AtomicU32>,
    sample_rate: u32,
    channels: u16,
    _stop: Sender<()>,
}

impl MicStream {
    /// Opens the device with the given id, or the default input when `None`.
    pub fn open(id: Option<&str>) -> anyhow::Result<Self> {
        let host = cpal::default_host();
        let device = match id {
            Some(id) => host
                .device_by_id(
                    &id.parse()
                        .map_err(|e| anyhow!("bad mic id {id:?}: {e:?}"))?,
                )
                .ok_or_else(|| anyhow!("microphone {id:?} not found"))?,
            None => host
                .default_input_device()
                .context("no default microphone")?,
        };
        let config = device
            .default_input_config()
            .context("querying microphone format")?;
        let (sample_rate, channels, format) = (
            config.sample_rate(),
            config.channels(),
            config.sample_format(),
        );

        let tap = Tap::default();
        let peak = Arc::new(AtomicU32::new(0));
        let (stop_tx, stop_rx) = bounded::<()>(0);
        let (ready_tx, ready_rx) = bounded(1);

        // cpal streams are not Send on every platform, so the stream lives on
        // its own thread until the MicStream is dropped.
        let (cb_tap, cb_peak) = (tap.clone(), peak.clone());
        thread::Builder::new()
            .name("visor-mic".into())
            .spawn(move || {
                let on_data = move |data: &Data, info: &InputCallbackInfo| {
                    let Some(samples) = to_f32(data, format) else {
                        return;
                    };
                    let ts = info.timestamp();
                    let latency = ts.callback.duration_since(ts.capture).as_micros() as Micros;
                    let level = samples.iter().fold(0f32, |m, s| m.max(s.abs())).min(1.0);
                    cb_peak.fetch_max(level.to_bits(), Ordering::Relaxed);
                    cb_tap.send(Arc::new(AudioBuffer {
                        host_ts: host_now().saturating_sub(latency),
                        sample_rate,
                        channels,
                        samples,
                    }));
                };
                let on_error = |e| tracing::error!("microphone stream error: {e}");
                let stream = device
                    .build_input_stream_raw(config.into(), format, on_data, on_error, None)
                    .context("opening microphone")
                    .and_then(|s| s.play().map(|()| s).context("starting microphone"));
                let ok = stream.is_ok();
                let _ = ready_tx.send(stream.map(drop));
                if ok {
                    // Blocks until the MicStream (and its stop sender) is dropped.
                    let _ = stop_rx.recv();
                }
            })?;
        ready_rx.recv().context("microphone thread exited")??;
        tracing::info!(sample_rate, channels, "microphone started");
        Ok(Self {
            tap,
            peak,
            sample_rate,
            channels,
            _stop: stop_tx,
        })
    }

    pub fn subscribe(&self) -> Receiver<Arc<AudioBuffer>> {
        self.tap.subscribe(256)
    }

    /// Peak input level since the previous call, 0.0–1.0.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Ordering::Relaxed))
    }

    pub fn format(&self) -> (u32, u16) {
        (self.sample_rate, self.channels)
    }
}

fn to_f32(data: &Data, format: SampleFormat) -> Option<Vec<f32>> {
    Some(match format {
        SampleFormat::F32 => data.as_slice::<f32>()?.to_vec(),
        SampleFormat::I16 => data
            .as_slice::<i16>()?
            .iter()
            .map(|&s| f32::from(s) / 32_768.0)
            .collect(),
        SampleFormat::I32 => data
            .as_slice::<i32>()?
            .iter()
            .map(|&s| s as f32 / 2_147_483_648.0)
            .collect(),
        SampleFormat::U16 => data
            .as_slice::<u16>()?
            .iter()
            .map(|&s| (f32::from(s) - 32_768.0) / 32_768.0)
            .collect(),
        other => {
            tracing::warn!(?other, "unsupported microphone sample format");
            return None;
        }
    })
}
