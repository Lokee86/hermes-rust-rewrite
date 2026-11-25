//! Microphone recording for platforms without a native dictation service. The recording is
//! transcribed on this computer and then discarded.
use super::Signal;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const RATE: u32 = 16_000;
const MAX: Duration = Duration::from_secs(300);
const PAUSE: Duration = Duration::from_millis(1500);
/// Louder than a quiet room, softer than speech.
const SPEECH: f32 = 0.015;

pub(super) struct Recording {
    stop: Arc<AtomicBool>,
    discard: Arc<AtomicBool>,
}

impl Recording {
    /// With `until_pause`, recording ends by itself after something was said.
    pub(super) fn start(data: PathBuf, locale: String, until_pause: bool, epoch: u64, sender: async_channel::Sender<(u64, Signal)>) -> Self {
        let (stop, discard) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let recording = Self { stop: stop.clone(), discard: discard.clone() };
        std::thread::spawn(move || {
            let signal = match record(&stop, until_pause) {
                _ if discard.load(Ordering::Acquire) => return,
                Ok(samples) if samples.is_empty() => Signal::Transcript(String::new(), true),
                Ok(samples) => match transcribe(&data, &locale, &samples) {
                    Ok(text) => Signal::Transcript(text, true),
                    Err(error) => Signal::Failure(error),
                },
                Err(error) => Signal::Failure(error),
            };
            let _ = sender.send_blocking((epoch, signal));
        });
        recording
    }
    pub(super) fn finish(&self) {
        self.stop.store(true, Ordering::Release);
    }
}
impl Drop for Recording {
    fn drop(&mut self) {
        // A finished recording is still being transcribed; only an unfinished one is cancelled.
        if !self.stop.swap(true, Ordering::AcqRel) {
            self.discard.store(true, Ordering::Release);
        }
    }
}

#[derive(Default)]
struct Captured {
    samples: Vec<f32>,
    heard: bool,
    last_heard: Option<Instant>,
    failure: Option<String>,
}

fn record(stop: &AtomicBool, until_pause: bool) -> Result<Vec<i16>, String> {
    let device = cpal::default_host().default_input_device().ok_or("No microphone was found.")?;
    let config = device.default_input_config().map_err(|error| format!("The microphone could not be opened: {error}"))?;
    let (rate, channels) = (config.sample_rate().0, usize::from(config.channels()).max(1));
    let captured = Arc::new(Mutex::new(Captured::default()));
    let failed = captured.clone();
    let on_error = move |error: cpal::StreamError| failed.lock().unwrap().failure = Some(format!("The microphone stopped: {error}"));
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => stream::<f32>(&device, &config.into(), channels, captured.clone(), on_error),
        cpal::SampleFormat::I16 => stream::<i16>(&device, &config.into(), channels, captured.clone(), on_error),
        cpal::SampleFormat::U16 => stream::<u16>(&device, &config.into(), channels, captured.clone(), on_error),
        cpal::SampleFormat::I32 => stream::<i32>(&device, &config.into(), channels, captured.clone(), on_error),
        other => return Err(format!("The microphone's format ({other}) is not supported.")),
    }?;
    stream.play().map_err(|error| format!("The microphone could not be started: {error}"))?;
    let started = Instant::now();
    while !stop.load(Ordering::Acquire) && started.elapsed() < MAX {
        std::thread::sleep(Duration::from_millis(50));
        let state = captured.lock().unwrap();
        if let Some(failure) = &state.failure {
            return Err(failure.clone());
        }
        if until_pause && state.last_heard.is_some_and(|at| at.elapsed() >= PAUSE) {
            break;
        }
    }
    drop(stream);
    let state = std::mem::take(&mut *captured.lock().unwrap());
    if !state.heard {
        return Ok(Vec::new());
    }
    Ok(resample(&state.samples, rate))
}

fn stream<T>(device: &cpal::Device, config: &cpal::StreamConfig, channels: usize, captured: Arc<Mutex<Captured>>, on_error: impl FnMut(cpal::StreamError) + Send + 'static) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    device.build_input_stream(config, move |data: &[T], _: &cpal::InputCallbackInfo| {
        let mono: Vec<f32> = data.chunks(channels)
            .map(|frame| frame.iter().map(|sample| <f32 as cpal::FromSample<T>>::from_sample_(*sample)).sum::<f32>() / frame.len() as f32)
            .collect();
        add(&mut captured.lock().unwrap(), &mono);
    }, on_error, None).map_err(|error| format!("The microphone could not be opened: {error}"))
}

fn add(captured: &mut Captured, mono: &[f32]) {
    if mono.is_empty() {
        return;
    }
    let level = (mono.iter().map(|sample| sample * sample).sum::<f32>() / mono.len() as f32).sqrt();
    if level >= SPEECH {
        captured.heard = true;
        captured.last_heard = Some(Instant::now());
    }
    captured.samples.extend_from_slice(mono);
}

/// Average down (or interpolate up) to the recognizer's rate.
fn resample(samples: &[f32], rate: u32) -> Vec<i16> {
    let step = f64::from(rate) / f64::from(RATE);
    let count = (samples.len() as f64 / step) as usize;
    (0..count).map(|index| {
        let from = (index as f64 * step) as usize;
        let to = (((index + 1) as f64 * step) as usize).clamp(from + 1, samples.len().max(from + 1));
        let window = &samples[from.min(samples.len() - 1)..to.min(samples.len())];
        let mean = window.iter().sum::<f32>() / window.len().max(1) as f32;
        (mean.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16
    }).collect()
}

fn wav(samples: &[i16]) -> Vec<u8> {
    let size = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + size as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + size).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&size.to_le_bytes());
    samples.iter().for_each(|sample| out.extend_from_slice(&sample.to_le_bytes()));
    out
}

fn transcribe(data: &Path, locale: &str, samples: &[i16]) -> Result<String, String> {
    let run = || -> anyhow::Result<String> {
        let file = tempfile::Builder::new().suffix(".wav").tempfile()?;
        std::fs::write(file.path(), wav(samples))?;
        let language = Some(locale.trim()).filter(|text| !text.is_empty());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        runtime.block_on(hermes_engine::transcribe::transcribe(data, file.path(), language))
    };
    run().map_err(|error| format!("{error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_is_converted_to_what_the_recognizer_takes() {
        let second: Vec<f32> = (0..48_000).map(|n| (n as f32 / 48_000.0 * 440.0 * std::f32::consts::TAU).sin() * 0.5).collect();
        let samples = resample(&second, 48_000);
        assert_eq!(samples.len(), 16_000);
        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!((14_000..=16_500).contains(&peak), "{peak}");
        assert_eq!(resample(&second[..8_000], 8_000).len(), 16_000);
        assert!(resample(&[], 44_100).is_empty());
        let file = wav(&samples);
        assert_eq!(&file[..4], b"RIFF");
        assert_eq!(file.len(), 44 + 32_000);
    }

    #[test]
    fn quiet_rooms_are_not_speech() {
        let mut captured = Captured::default();
        add(&mut captured, &[0.002; 480]);
        assert!(!captured.heard);
        add(&mut captured, &[0.2; 480]);
        assert!(captured.heard && captured.last_heard.is_some());
    }
}
