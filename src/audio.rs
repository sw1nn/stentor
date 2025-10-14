use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Sample, SampleFormat, Stream, StreamConfig};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

pub struct AudioRecorder {
    #[allow(dead_code)]
    sample_rate: u32,
    silence_threshold: f32,
    device: Device,
    config: StreamConfig,
}

#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub data: Vec<f32>,
    pub rms: f32,
}

pub enum RecordingCommand {
    Stop,
}

impl AudioRecorder {
    pub fn new(sample_rate: u32, silence_threshold: f32) -> Result<Self> {
        let host = cpal::default_host();

        let device = host
            .default_input_device()
            .context("No input device available")?;

        log::info!("Using audio device: {}", device.name()?);

        let config = device
            .default_input_config()
            .context("Failed to get default input config")?;

        log::info!("Sample format: {:?}", config.sample_format());
        log::info!("Sample rate: {}", config.sample_rate().0);
        log::info!("Channels: {}", config.channels());

        // Create a config with our desired sample rate
        let config = StreamConfig {
            channels: 1,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Fixed(1024),
        };

        Ok(Self {
            sample_rate,
            silence_threshold,
            device,
            config,
        })
    }

    pub fn get_device_name(&self) -> Result<String> {
        self.device.name().context("Failed to get device name")
    }

    pub fn start_recording(
        &self,
        chunk_tx: Sender<AudioChunk>,
        cmd_rx: Arc<Mutex<Receiver<RecordingCommand>>>,
    ) -> Result<Stream> {
        let silence_threshold = self.silence_threshold;

        let stream = match self.device.default_input_config()?.sample_format() {
            SampleFormat::F32 => self.build_stream::<f32>(chunk_tx, cmd_rx, silence_threshold)?,
            SampleFormat::I16 => self.build_stream::<i16>(chunk_tx, cmd_rx, silence_threshold)?,
            SampleFormat::U16 => self.build_stream::<u16>(chunk_tx, cmd_rx, silence_threshold)?,
            format => anyhow::bail!("Unsupported sample format: {:?}", format),
        };

        stream.play().context("Failed to start audio stream")?;

        Ok(stream)
    }

    fn build_stream<T>(
        &self,
        chunk_tx: Sender<AudioChunk>,
        cmd_rx: Arc<Mutex<Receiver<RecordingCommand>>>,
        _silence_threshold: f32,
    ) -> Result<Stream>
    where
        T: Sample + cpal::SizedSample,
        f32: cpal::FromSample<T>,
    {
        let err_fn = |err| {
            log::error!("Audio stream error: {}", err);
        };

        let chunk_tx = Arc::new(Mutex::new(Some(chunk_tx)));
        let chunk_tx_clone = Arc::clone(&chunk_tx);

        let stream = self.device.build_input_stream(
            &self.config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                // Check for stop command
                if let Ok(rx) = cmd_rx.lock() {
                    if let Ok(RecordingCommand::Stop) = rx.try_recv() {
                        log::info!("Stop command received, closing audio stream");
                        // Drop the sender to signal the receiver
                        if let Ok(mut tx_opt) = chunk_tx_clone.lock() {
                            *tx_opt = None;
                        }
                        return;
                    }
                }

                // Convert samples to f32
                let samples: Vec<f32> = data.iter().map(|&s| s.to_sample::<f32>()).collect();

                // Calculate RMS
                let rms = calculate_rms(&samples);

                // Send chunk
                let chunk = AudioChunk {
                    data: samples,
                    rms,
                };

                if let Ok(tx_opt) = chunk_tx.lock() {
                    if let Some(ref tx) = *tx_opt {
                        if tx.send(chunk).is_err() {
                            log::warn!("Failed to send audio chunk (receiver dropped)");
                        }
                    }
                }
            },
            err_fn,
            None,
        )?;

        Ok(stream)
    }
}

pub struct VoiceActivityDetector {
    silence_threshold: f32,
    silence_duration: f32,
    min_speech_duration: f32,
    sample_rate: u32,
    samples_per_chunk: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadState {
    Idle,
    Speaking,
    SilenceAfterSpeech,
}

pub struct VadResult {
    pub state: VadState,
    pub should_stop: bool,
    #[allow(dead_code)]
    pub has_minimum_speech: bool,
}

impl VoiceActivityDetector {
    pub fn new(
        silence_threshold: f32,
        silence_duration: f32,
        min_speech_duration: f32,
        sample_rate: u32,
    ) -> Self {
        Self {
            silence_threshold,
            silence_duration,
            min_speech_duration,
            sample_rate,
            samples_per_chunk: 1024,
        }
    }

    pub fn process_chunk(
        &self,
        rms: f32,
        current_state: VadState,
        silence_chunks: usize,
        speech_chunks: usize,
    ) -> VadResult {
        let silence_chunk_threshold =
            (self.silence_duration * self.sample_rate as f32 / self.samples_per_chunk as f32)
                as usize;
        let min_speech_chunk_threshold =
            (self.min_speech_duration * self.sample_rate as f32 / self.samples_per_chunk as f32)
                as usize;

        let is_speech = rms > self.silence_threshold;
        let has_minimum_speech = speech_chunks >= min_speech_chunk_threshold;

        match current_state {
            VadState::Idle => {
                if is_speech {
                    VadResult {
                        state: VadState::Speaking,
                        should_stop: false,
                        has_minimum_speech: false,
                    }
                } else {
                    VadResult {
                        state: VadState::Idle,
                        should_stop: false,
                        has_minimum_speech: false,
                    }
                }
            }
            VadState::Speaking => {
                if is_speech {
                    VadResult {
                        state: VadState::Speaking,
                        should_stop: false,
                        has_minimum_speech,
                    }
                } else {
                    VadResult {
                        state: VadState::SilenceAfterSpeech,
                        should_stop: false,
                        has_minimum_speech,
                    }
                }
            }
            VadState::SilenceAfterSpeech => {
                if is_speech {
                    VadResult {
                        state: VadState::Speaking,
                        should_stop: false,
                        has_minimum_speech,
                    }
                } else if silence_chunks >= silence_chunk_threshold {
                    VadResult {
                        state: VadState::SilenceAfterSpeech,
                        should_stop: has_minimum_speech,
                        has_minimum_speech,
                    }
                } else {
                    VadResult {
                        state: VadState::SilenceAfterSpeech,
                        should_stop: false,
                        has_minimum_speech,
                    }
                }
            }
        }
    }
}

fn calculate_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }

    let sum_squares: f32 = samples.iter().map(|&s| s * s).sum();
    (sum_squares / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_rms() {
        let samples = vec![0.1, -0.1, 0.2, -0.2];
        let rms = calculate_rms(&samples);
        assert!(rms > 0.0);
        assert!(rms < 1.0);
    }

    #[test]
    fn test_vad_idle_to_speaking() {
        let vad = VoiceActivityDetector::new(0.01, 1.5, 0.5, 16000);
        let result = vad.process_chunk(0.05, VadState::Idle, 0, 0);
        assert_eq!(result.state, VadState::Speaking);
        assert!(!result.should_stop);
    }

    #[test]
    fn test_vad_speaking_to_silence() {
        let vad = VoiceActivityDetector::new(0.01, 1.5, 0.5, 16000);
        let result = vad.process_chunk(0.005, VadState::Speaking, 0, 10);
        assert_eq!(result.state, VadState::SilenceAfterSpeech);
        assert!(!result.should_stop);
    }
}
