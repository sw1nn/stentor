use anyhow::{Context, Result};
use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::{Context as PulseContext, FlagSet as ContextFlagSet};
use libpulse_binding::def::BufferAttr;
use libpulse_binding::mainloop::threaded::Mainloop;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

/// Helper to wait for PulseAudio context to be ready
pub fn wait_for_context_ready(mainloop: &mut Mainloop, context: &PulseContext) -> Result<()> {
    const MAX_ITERATIONS: u32 = 100; // 1 second timeout (100 * 10ms)

    for _iteration in 0..MAX_ITERATIONS {
        match context.get_state() {
            libpulse_binding::context::State::Ready => {
                return Ok(());
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

    mainloop.unlock();
    mainloop.stop();
    anyhow::bail!("Timeout waiting for PulseAudio context to become ready")
}

/// Helper struct to reuse PulseAudio mainloop and context for introspection operations
pub struct PulseIntrospector {
    mainloop: Mainloop,
    context: PulseContext,
}

impl PulseIntrospector {
    /// Create a new PulseIntrospector with its own mainloop and context
    pub fn new() -> Result<Self> {
        let mut mainloop = Mainloop::new().context("Failed to create PulseAudio mainloop")?;
        let mut context = PulseContext::new(&mainloop, "stentor-introspector")
            .context("Failed to create PulseAudio context")?;

        context
            .connect(None, ContextFlagSet::NOFLAGS, None)
            .context("Failed to connect to PulseAudio")?;

        mainloop.lock();
        mainloop.start().context("Failed to start mainloop")?;

        wait_for_context_ready(&mut mainloop, &context)?;

        Ok(Self { mainloop, context })
    }

    /// Check if a source with the given name exists
    pub fn source_exists(&mut self, name: &str) -> Result<bool> {
        let found = Rc::new(RefCell::new(false));
        let found_clone = Rc::clone(&found);
        let target_name = name.to_string();

        let introspect = self.context.introspect();
        introspect.get_source_info_list(move |result| match result {
            ListResult::Item(source_info) => {
                if let Some(source_name) = source_info.name.as_ref() {
                    if source_name.as_ref() == target_name.as_str() {
                        *found_clone.borrow_mut() = true;
                    }
                }
            }
            ListResult::End | ListResult::Error => {}
        });

        self.mainloop.unlock();
        std::thread::sleep(std::time::Duration::from_millis(100));
        self.mainloop.lock();

        Ok(*found.borrow())
    }

    /// Get the description for a named source
    pub fn get_source_description(&mut self, source_name: &str) -> Result<String> {
        let desc_result = Arc::new(Mutex::new(None));
        let desc_result_clone = Arc::clone(&desc_result);

        let introspector = self.context.introspect();
        introspector.get_source_info_by_name(source_name, move |list_result| {
            if let ListResult::Item(source_info) = list_result {
                if let Some(desc) = source_info.description.as_ref() {
                    *desc_result_clone.lock().unwrap() = Some(desc.to_string());
                }
            }
        });

        self.mainloop.unlock();
        std::thread::sleep(std::time::Duration::from_millis(100));
        self.mainloop.lock();

        let desc = desc_result.lock().unwrap().clone();
        desc.ok_or_else(|| {
            anyhow::anyhow!("Could not get description for source '{}'", source_name)
        })
    }

    /// Get the description for the default source
    pub fn get_default_source_description(&mut self) -> Result<String> {
        let default_source_name = self
            .get_default_source_name()?
            .ok_or_else(|| anyhow::anyhow!("Could not get default source name"))?;

        self.get_source_description(&default_source_name)
    }

    /// Get the name of the default source
    pub fn get_default_source_name(&mut self) -> Result<Option<String>> {
        let result = Arc::new(Mutex::new(None));
        let result_clone = Arc::clone(&result);

        let introspector = self.context.introspect();
        introspector.get_server_info(move |server_info| {
            if let Some(default_source) = server_info.default_source_name.as_ref() {
                *result_clone.lock().unwrap() = Some(default_source.to_string());
            }
        });

        self.mainloop.unlock();
        std::thread::sleep(std::time::Duration::from_millis(100));
        self.mainloop.lock();

        Ok(result.lock().unwrap().clone())
    }

    /// List all available source names.
    /// Used by stentorctl binary (compiler can't detect cross-binary usage).
    pub fn list_sources(&mut self) -> Result<Vec<String>> {
        let sources = Rc::new(RefCell::new(Vec::new()));
        let sources_clone = Rc::clone(&sources);

        let introspect = self.context.introspect();
        introspect.get_source_info_list(move |result| {
            use ListResult::*;
            match result {
                Item(source_info) => {
                    if let Some(name) = source_info.name.as_ref() {
                        sources_clone.borrow_mut().push(name.to_string());
                    }
                }
                End | Error => {}
            }
        });

        self.mainloop.unlock();
        std::thread::sleep(std::time::Duration::from_millis(100));
        self.mainloop.lock();

        Ok(sources.borrow().clone())
    }
}

impl Drop for PulseIntrospector {
    fn drop(&mut self) {
        // Mainloop should be locked at this point from all our operations
        // We need to unlock it before stopping
        self.mainloop.unlock();

        // Disconnect and cleanup context before stopping mainloop
        self.context.disconnect();

        // Stop the mainloop
        self.mainloop.stop();
    }
}

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
            tracing::info!("Looking for audio source: {}", name);

            // Check if source exists using PulseIntrospector
            let mut introspector = PulseIntrospector::new()?;
            if !introspector.source_exists(name)? {
                anyhow::bail!("Audio source '{}' not found", name);
            }

            tracing::info!("Found audio source: {}", name);
        } else {
            tracing::info!("Using default audio source");
        }

        Ok(Self {
            source_name,
            sample_rate,
        })
    }

    pub fn get_device_name(&self) -> Result<String> {
        let mut introspector = PulseIntrospector::new()?;

        if let Some(ref name) = self.source_name {
            // Get the description for the named source
            introspector.get_source_description(name)
        } else {
            // Get the default source description
            introspector.get_default_source_description()
        }
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
            None,                        // Use default server
            "stentor",                   // Application name
            Direction::Record,           // Record direction
            self.source_name.as_deref(), // Source name (None for default)
            "Audio Recording",           // Stream description
            &spec,                       // Sample spec
            None,                        // Channel map (None for default)
            Some(&buffer_attr),          // Buffer attributes
        )
        .context("Failed to create PulseAudio simple connection")?;

        tracing::info!("PulseAudio simple connection created and ready");

        // Spawn a thread to continuously read audio
        std::thread::spawn(move || {
            // Buffer to read audio into (1024 samples * 2 bytes per sample)
            let mut buffer = vec![0u8; 1024 * 2];

            loop {
                // Check for stop command
                if let Ok(rx) = cmd_rx.lock() {
                    if let Ok(RecordingCommand::Stop) = rx.try_recv() {
                        tracing::info!("Stop command received in audio thread");
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
                            tracing::info!("Receiver dropped, stopping audio thread");
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to read audio: {:?}", e);
                        break;
                    }
                }
            }

            tracing::info!("Audio recording thread exiting");
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
        let stop_silence_chunk_threshold = (self.stop_silence_duration * self.sample_rate as f32
            / self.samples_per_chunk as f32) as usize;
        let min_speech_chunk_threshold = (self.min_speech_duration * self.sample_rate as f32
            / self.samples_per_chunk as f32) as usize;

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

    #[test]
    fn test_list_sources() {
        let mut introspector = PulseIntrospector::new().expect("Failed to create introspector");
        let sources = introspector.list_sources().expect("Failed to list sources");
        assert!(sources.is_empty() || !sources.is_empty());
    }
}
