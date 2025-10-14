use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Sample, SampleFormat, Stream, StreamConfig};
use libpulse_binding::context::{Context as PulseContext, FlagSet as ContextFlagSet};
use libpulse_binding::mainloop::threaded::Mainloop;
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
    pub fn new(sample_rate: u32, silence_threshold: f32, device_name: Option<String>) -> Result<Self> {
        let host = cpal::default_host();

        let device = if let Some(name) = device_name {
            // Find device by name
            log::info!("Looking for audio device: {}", name);
            host.input_devices()
                .context("Failed to enumerate input devices")?
                .find(|d| {
                    if let Ok(device_name) = d.name() {
                        device_name == name
                    } else {
                        false
                    }
                })
                .with_context(|| format!("Audio device '{}' not found", name))?
        } else {
            // Use default device
            host.default_input_device()
                .context("No input device available")?
        };

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
        let device_name = self.device.name().context("Failed to get device name")?;

        // If the device is "default", also get the actual PulseAudio source description
        if device_name.to_lowercase() == "default" {
            if let Ok(desc) = get_pulse_default_source_description() {
                return Ok(format!("{} ({})", device_name, desc));
            }
        }

        Ok(device_name)
    }

    pub fn get_actual_sample_rate(&self) -> Result<u32> {
        let config = self.device.default_input_config()?;
        Ok(config.sample_rate().0)
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
    min_speech_duration: f32,
    stop_silence_duration: f32,  // Silence duration to trigger stop
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
        min_speech_duration: f32,
        stop_silence_duration: f32,
        sample_rate: u32,
    ) -> Self {
        Self {
            silence_threshold,
            min_speech_duration,
            stop_silence_duration,
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
        let stop_silence_chunk_threshold =
            (self.stop_silence_duration * self.sample_rate as f32 / self.samples_per_chunk as f32)
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
                } else if silence_chunks >= stop_silence_chunk_threshold {
                    // Use the longer stop_silence_duration threshold
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

fn get_pulse_default_source_description() -> Result<String> {
    let mut mainloop = Mainloop::new().context("Failed to create PulseAudio mainloop")?;
    let mut context = PulseContext::new(&mainloop, "stentor-query")
        .context("Failed to create PulseAudio context")?;

    context
        .connect(None, ContextFlagSet::NOFLAGS, None)
        .context("Failed to connect to PulseAudio")?;

    mainloop.lock();
    mainloop.start().context("Failed to start mainloop")?;

    // Wait for context to be ready
    loop {
        match context.get_state() {
            libpulse_binding::context::State::Ready => break,
            libpulse_binding::context::State::Failed | libpulse_binding::context::State::Terminated => {
                mainloop.unlock();
                mainloop.stop();
                anyhow::bail!("PulseAudio context failed");
            }
            _ => {
                mainloop.unlock();
                std::thread::sleep(std::time::Duration::from_millis(10));
                mainloop.lock();
            }
        }
    }

    let result = Arc::new(Mutex::new(None));
    let result_clone = Arc::clone(&result);

    // Get server info to find default source
    let introspector = context.introspect();
    introspector.get_server_info(move |server_info| {
        if let Some(default_source) = server_info.default_source_name.as_ref() {
            *result_clone.lock().unwrap() = Some(default_source.to_string());
        }
    });

    mainloop.unlock();

    // Wait for callback
    std::thread::sleep(std::time::Duration::from_millis(100));

    mainloop.lock();
    let default_source_name = result.lock().unwrap().clone();
    mainloop.unlock();

    if let Some(source_name) = default_source_name {
        let desc_result = Arc::new(Mutex::new(None));
        let desc_result_clone = Arc::clone(&desc_result);

        mainloop.lock();
        let introspector = context.introspect();
        introspector.get_source_info_by_name(&source_name, move |list_result| {
            if let libpulse_binding::callbacks::ListResult::Item(source_info) = list_result {
                if let Some(desc) = source_info.description.as_ref() {
                    *desc_result_clone.lock().unwrap() = Some(desc.to_string());
                }
            }
        });
        mainloop.unlock();

        // Wait for callback
        std::thread::sleep(std::time::Duration::from_millis(100));

        mainloop.stop();

        let desc = desc_result.lock().unwrap().clone();
        if let Some(desc) = desc {
            return Ok(desc);
        }
    }

    mainloop.stop();
    anyhow::bail!("Could not get default source description")
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
        let vad = VoiceActivityDetector::new(0.01, 0.5, 1.5, 16000);
        let result = vad.process_chunk(0.05, VadState::Idle, 0, 0);
        assert_eq!(result.state, VadState::Speaking);
        assert!(!result.should_stop);
    }

    #[test]
    fn test_vad_speaking_to_silence() {
        let vad = VoiceActivityDetector::new(0.01, 0.5, 1.5, 16000);
        let result = vad.process_chunk(0.005, VadState::Speaking, 0, 10);
        assert_eq!(result.state, VadState::SilenceAfterSpeech);
        assert!(!result.should_stop);
    }
}
