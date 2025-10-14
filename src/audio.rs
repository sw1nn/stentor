use anyhow::{Context, Result};
use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::{Context as PulseContext, FlagSet as ContextFlagSet};
use libpulse_binding::mainloop::threaded::Mainloop;
use libpulse_binding::sample::{Format, Spec};
use libpulse_simple_binding::Simple;
use libpulse_binding::stream::Direction;
use libpulse_binding::def::BufferAttr;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

pub struct AudioRecorder {
    source_name: Option<String>,
    sample_rate: u32,
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
    pub fn new(sample_rate: u32, source_name: Option<String>) -> Result<Self> {
        // Validate that the source exists if specified
        if let Some(ref name) = source_name {
            log::info!("Looking for audio source: {}", name);

            // Check if source exists
            if !Self::source_exists(name)? {
                anyhow::bail!("Audio source '{}' not found", name);
            }

            log::info!("Found audio source: {}", name);
        } else {
            log::info!("Using default audio source");
        }

        Ok(Self {
            source_name,
            sample_rate,
        })
    }

    /// Check if a source with the given name exists
    fn source_exists(name: &str) -> Result<bool> {
        let mut mainloop = Mainloop::new().context("Failed to create PulseAudio mainloop")?;
        let mut context = PulseContext::new(&mainloop, "stentor-source-check")
            .context("Failed to create PulseAudio context")?;

        context
            .connect(None, ContextFlagSet::NOFLAGS, None)
            .context("Failed to connect to PulseAudio")?;

        mainloop.lock();
        mainloop.start().context("Failed to start mainloop")?;

        // Wait for context to be ready (with timeout)
        const MAX_ITERATIONS: u32 = 100; // 1 second timeout (100 * 10ms)
        let mut ready = false;

        for _iteration in 0..MAX_ITERATIONS {
            match context.get_state() {
                libpulse_binding::context::State::Ready => {
                    ready = true;
                    break;
                }
                libpulse_binding::context::State::Failed
                | libpulse_binding::context::State::Terminated => {
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

        if !ready {
            mainloop.unlock();
            mainloop.stop();
            anyhow::bail!("Timeout waiting for PulseAudio context to become ready");
        }

        let found = Rc::new(RefCell::new(false));
        let found_clone = Rc::clone(&found);
        let target_name = name.to_string();

        let introspect = context.introspect();
        introspect.get_source_info_list(move |result| match result {
            ListResult::Item(source_info) => {
                if let Some(source_name) = source_info.name.as_ref() {
                    if source_name.as_ref() == target_name.as_str() {
                        *found_clone.borrow_mut() = true;
                    }
                }
            }
            ListResult::End => {}
            ListResult::Error => {}
        });

        mainloop.unlock();
        std::thread::sleep(std::time::Duration::from_millis(100));

        mainloop.lock();
        let exists = *found.borrow();
        mainloop.unlock();

        mainloop.stop();

        Ok(exists)
    }

    pub fn get_device_name(&self) -> Result<String> {
        if let Some(ref name) = self.source_name {
            // Get the description for the named source
            return get_pulse_source_description(name);
        }

        // Get the default source description
        get_pulse_default_source_description()
    }

    pub fn get_actual_sample_rate(&self) -> Result<u32> {
        // PulseAudio will resample to our requested rate
        Ok(self.sample_rate)
    }

    pub fn start_recording(
        &self,
        chunk_tx: Sender<AudioChunk>,
        cmd_rx: Arc<Mutex<Receiver<RecordingCommand>>>,
    ) -> Result<()> {
        // Create sample spec
        let spec = Spec {
            format: Format::S16le,
            channels: 1,
            rate: self.sample_rate,
        };

        if !spec.is_valid() {
            anyhow::bail!("Invalid sample spec");
        }

        // Set buffer attributes
        let buffer_attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: 1024 * 2, // Request 1024 samples at a time (in bytes, so *2 for s16)
        };

        // Create simple PulseAudio connection
        let simple = Simple::new(
            None,                           // Use default server
            "stentor",                      // Application name
            Direction::Record,              // Record direction
            self.source_name.as_deref(),    // Source name (None for default)
            "Audio Recording",              // Stream description
            &spec,                          // Sample spec
            None,                           // Channel map (None for default)
            Some(&buffer_attr),             // Buffer attributes
        ).context("Failed to create PulseAudio simple connection")?;

        log::info!("PulseAudio simple connection created and ready");

        // Spawn a thread to continuously read audio
        std::thread::spawn(move || {
            // Buffer to read audio into (1024 samples * 2 bytes per sample)
            let mut buffer = vec![0u8; 1024 * 2];

            loop {
                // Check for stop command
                if let Ok(rx) = cmd_rx.lock() {
                    if let Ok(RecordingCommand::Stop) = rx.try_recv() {
                        log::info!("Stop command received in audio thread");
                        break;
                    }
                }

                // Read audio data (this blocks until data is available)
                match simple.read(&mut buffer) {
                    Ok(()) => {
                        // Convert i16 samples to f32
                        let samples: Vec<f32> = buffer
                            .chunks_exact(2)
                            .map(|chunk| {
                                let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                                sample as f32 / 32768.0
                            })
                            .collect();

                        // Calculate RMS
                        let rms = calculate_rms(&samples);

                        // Send chunk
                        let chunk = AudioChunk { data: samples, rms };

                        if chunk_tx.send(chunk).is_err() {
                            log::info!("Receiver dropped, stopping audio thread");
                            break;
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to read audio: {:?}", e);
                        break;
                    }
                }
            }

            log::info!("Audio recording thread exiting");
        });

        Ok(())
    }
}

pub struct VoiceActivityDetector {
    silence_threshold: f32,
    min_speech_duration: f32,
    stop_silence_duration: f32, // Silence duration to trigger stop
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

fn get_pulse_source_description(source_name: &str) -> Result<String> {
    let mut mainloop = Mainloop::new().context("Failed to create PulseAudio mainloop")?;
    let mut context = PulseContext::new(&mainloop, "stentor-query")
        .context("Failed to create PulseAudio context")?;

    context
        .connect(None, ContextFlagSet::NOFLAGS, None)
        .context("Failed to connect to PulseAudio")?;

    mainloop.lock();
    mainloop.start().context("Failed to start mainloop")?;

    // Wait for context to be ready (with timeout)
    const MAX_ITERATIONS: u32 = 100; // 1 second timeout (100 * 10ms)
    let mut ready = false;

    for _iteration in 0..MAX_ITERATIONS {
        match context.get_state() {
            libpulse_binding::context::State::Ready => {
                ready = true;
                break;
            }
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

    if !ready {
        mainloop.unlock();
        mainloop.stop();
        anyhow::bail!("Timeout waiting for PulseAudio context to become ready");
    }

    let desc_result = Arc::new(Mutex::new(None));
    let desc_result_clone = Arc::clone(&desc_result);

    let introspector = context.introspect();
    introspector.get_source_info_by_name(source_name, move |list_result| {
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
        Ok(desc)
    } else {
        anyhow::bail!("Could not get description for source '{}'", source_name)
    }
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

    // Wait for context to be ready (with timeout)
    const MAX_ITERATIONS: u32 = 100; // 1 second timeout (100 * 10ms)
    let mut ready = false;

    for _iteration in 0..MAX_ITERATIONS {
        match context.get_state() {
            libpulse_binding::context::State::Ready => {
                ready = true;
                break;
            }
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

    if !ready {
        mainloop.unlock();
        mainloop.stop();
        anyhow::bail!("Timeout waiting for PulseAudio context to become ready");
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
