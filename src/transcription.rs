use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use whisper_rs::{
    FullParams, SamplingStrategy, SegmentCallbackData, WhisperContext, WhisperContextParameters,
};

pub struct Transcriber {
    context: WhisperContext,
    language: Option<String>,
}

impl Transcriber {
    /// Create a new transcriber with the specified model
    ///
    /// Model files should be in GGML format and located at:
    /// ~/.local/share/whisper/ggml-{model_size}.bin
    pub fn new(model_size: String, language: String) -> Result<Self> {
        let model_path = Self::get_model_path(&model_size)?;

        log::info!("Loading Whisper model from: {}", model_path.display());

        // Suppress verbose whisper.cpp output by installing logging hooks
        // Without a logging backend feature enabled, this effectively disables whisper logging
        whisper_rs::install_logging_hooks();

        let ctx_params = WhisperContextParameters::default();
        let context = WhisperContext::new_with_params(&model_path.to_string_lossy(), ctx_params)
            .context("Failed to load Whisper model")?;

        let language = if language == "auto" {
            None
        } else {
            Some(language)
        };

        Ok(Self { context, language })
    }

    /// Get the path to the Whisper model file, downloading if necessary
    fn get_model_path(model_size: &str) -> Result<PathBuf> {
        // Determine the target directory for models
        let xdg_dirs = xdg::BaseDirectories::new();
        let model_dir = if let Some(data_home) = xdg_dirs.get_data_home() {
            if let Some(parent) = data_home.parent() {
                parent.join("whisper")
            } else {
                PathBuf::from(std::env::var("HOME")?)
                    .join(".local")
                    .join("share")
                    .join("whisper")
            }
        } else {
            PathBuf::from(std::env::var("HOME")?)
                .join(".local")
                .join("share")
                .join("whisper")
        };

        let model_path = model_dir.join(format!("ggml-{}.bin", model_size));

        // If model exists, return it
        if model_path.exists() {
            return Ok(model_path);
        }

        // Try current directory as fallback
        let local_model_path = PathBuf::from(format!("ggml-{}.bin", model_size));
        if local_model_path.exists() {
            return Ok(local_model_path);
        }

        // Model doesn't exist, download it
        log::info!("Model not found, downloading ggml-{}.bin...", model_size);
        Self::download_model(model_size, &model_path)?;

        Ok(model_path)
    }

    /// Download a Whisper model from HuggingFace
    fn download_model(model_size: &str, target_path: &PathBuf) -> Result<()> {
        // Create directory if it doesn't exist
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).context("Failed to create model directory")?;
        }

        // Construct download URL
        let url = format!(
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{}.bin",
            model_size
        );

        log::info!("Downloading from: {}", url);
        log::info!("Saving to: {}", target_path.display());

        // Download the model
        let response = reqwest::blocking::get(&url)
            .with_context(|| format!("Failed to download model from {}", url))?;

        if !response.status().is_success() {
            anyhow::bail!(
                "Failed to download model: HTTP {}. Model '{}' may not exist.",
                response.status(),
                model_size
            );
        }

        // Get total size for progress tracking
        let total_size = response
            .content_length()
            .context("Failed to get content length")?;

        // Create progress bar
        let pb = ProgressBar::new(total_size);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{msg}\n{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})")
                .unwrap()
                .progress_chars("#>-"),
        );
        pb.set_message(format!("Downloading ggml-{}.bin", model_size));

        // Write to file with streaming progress
        let file = fs::File::create(target_path)
            .with_context(|| format!("Failed to create file: {}", target_path.display()))?;

        // Stream the response body in chunks
        use std::io::copy;
        let mut content = response;

        // Wrap the file writer with a progress updater
        struct ProgressWriter<W: Write> {
            writer: W,
            progress_bar: ProgressBar,
        }

        impl<W: Write> Write for ProgressWriter<W> {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                let written = self.writer.write(buf)?;
                self.progress_bar.inc(written as u64);
                Ok(written)
            }

            fn flush(&mut self) -> io::Result<()> {
                self.writer.flush()
            }
        }

        let mut progress_writer = ProgressWriter {
            writer: file,
            progress_bar: pb.clone(),
        };

        copy(&mut content, &mut progress_writer).context("Failed to write response to file")?;

        pb.finish_with_message(format!("Downloaded ggml-{}.bin successfully!", model_size));

        Ok(())
    }

    /// Transcribe audio data with a callback for real-time segment updates
    ///
    /// Audio should be mono, 16kHz sample rate, f32 format
    /// The callback is called in real-time as each segment is decoded during transcription
    pub fn transcribe_with_realtime_callback<F>(
        &self,
        audio_data: &[f32],
        mut callback: F,
    ) -> Result<String>
    where
        F: FnMut(&str) + Send + 'static,
    {
        // Create state
        let mut state = self
            .context
            .create_state()
            .context("Failed to create Whisper state")?;

        // Configure transcription parameters
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });

        // Set language if specified
        if let Some(ref lang) = self.language {
            params.set_language(Some(lang));
        }

        // Disable printing to console
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        // Set up real-time segment callback
        params.set_segment_callback_safe(move |segment_data: SegmentCallbackData| {
            callback(&segment_data.text);
        });

        // Process audio
        let result = state
            .full(params, audio_data)
            .context("Failed to transcribe audio");

        result?;

        // Extract complete transcribed text
        let num_segments = state.full_n_segments();

        let mut text = String::new();
        for i in 0..num_segments {
            if let Some(segment) = state.get_segment(i) {
                if let Ok(segment_text) = segment.to_str_lossy() {
                    text.push_str(&segment_text);
                }
            }
        }

        Ok(text.trim().to_string())
    }

    /// Transcribe audio data
    ///
    /// Audio should be mono, 16kHz sample rate, f32 format
    #[allow(dead_code)]
    pub fn transcribe(&self, audio_data: &[f32]) -> Result<String> {
        // Convert audio to the format whisper-rs expects
        let mut state = self
            .context
            .create_state()
            .context("Failed to create Whisper state")?;

        // Configure transcription parameters
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });

        // Set language if specified
        if let Some(ref lang) = self.language {
            params.set_language(Some(lang));
        }

        // Disable printing to console
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        // Process audio
        let result = state
            .full(params, audio_data)
            .context("Failed to transcribe audio");

        result?;

        // Extract transcribed text
        let num_segments = state.full_n_segments();

        let mut text = String::new();
        for i in 0..num_segments {
            if let Some(segment) = state.get_segment(i) {
                if let Ok(segment_text) = segment.to_str_lossy() {
                    text.push_str(&segment_text);
                }
            }
        }

        Ok(text.trim().to_string())
    }

    /// Get available languages
    #[allow(dead_code)]
    pub fn get_language(&self) -> Option<&str> {
        self.language.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore] // Requires model file to be present
    fn test_transcriber_creation() {
        let result = Transcriber::new("tiny".to_string(), "en".to_string());
        assert!(result.is_ok());
    }

    #[test]
    fn test_model_path_construction() {
        // This will fail if model doesn't exist, which is expected
        let result = Transcriber::get_model_path("tiny");
        // Just check that the function runs without panicking
        let _ = result;
    }
}
