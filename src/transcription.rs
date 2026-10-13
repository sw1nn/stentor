use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use parking_lot::Mutex;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

pub struct Transcriber {
    language: Option<String>,
    /// Reused across calls. Guarded by a mutex; only one transcription runs at
    /// a time. The state keeps an internal reference to the context alive.
    state: Mutex<WhisperState>,
    /// Decoder worker threads.
    n_threads: i32,
}

/// Worker threads for decoding. whisper.cpp defaults to `min(4, cores)`, which
/// underuses larger CPUs.
fn thread_count() -> i32 {
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, 8) as i32)
        .unwrap_or(4)
}

/// One transcribed segment, with its end time relative to the audio passed in.
pub struct TranscribedSegment {
    /// End time in centiseconds (10 ms).
    pub end_cs: i64,
    pub text: String,
}

/// Encoder-context sizes (in encoder frames) used to bound the number of
/// distinct GPU compute graphs. Each bucket covers `bucket * 320` samples.
const AUDIO_CTX_BUCKETS: [i32; 4] = [256, 512, 1024, 1500];

/// Smallest encoder-context bucket that covers `samples` samples.
///
/// The mel has one frame per 160 samples and the encoder consumes two frames
/// per context position, so covering `samples` needs `samples / 320`.
fn audio_ctx_for_samples(samples: usize) -> i32 {
    let needed = samples.div_ceil(320) as i32 + 1;
    AUDIO_CTX_BUCKETS
        .iter()
        .copied()
        .find(|bucket| *bucket >= needed)
        .unwrap_or(1500)
}

impl Transcriber {
    /// Create a new transcriber with the specified model
    ///
    /// Model files should be in GGML format and located at:
    /// ~/.local/share/whisper/ggml-{model_size}.bin
    pub fn new<M, L>(model_size: M, language: L) -> Result<Self>
    where
        M: Into<String>,
        L: Into<String>,
    {
        let model_size = model_size.into();
        let language = language.into();
        let model_path = Self::get_model_path(&model_size)?;

        tracing::info!("Loading Whisper model from: {}", model_path.display());

        // Route whisper.cpp logs through tracing if `whisper-tracing-backend` feature is enabled,
        // otherwise this suppresses all whisper.cpp output
        whisper_rs::install_logging_hooks();

        let ctx_params = WhisperContextParameters::default();
        let context = WhisperContext::new_with_params(&model_path, ctx_params)
            .context("Failed to load Whisper model")?;

        let state = context
            .create_state()
            .context("Failed to create Whisper state")?;

        let language = if language == "auto" {
            None
        } else {
            Some(language)
        };

        Ok(Self {
            language,
            state: Mutex::new(state),
            n_threads: thread_count(),
        })
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
        tracing::info!("Model not found, downloading ggml-{}.bin...", model_size);
        Self::download_model(model_size, &model_path)?;

        Ok(model_path)
    }

    /// Download a Whisper model from HuggingFace
    fn download_model<P>(model_size: &str, target_path: P) -> Result<()>
    where
        P: AsRef<Path>,
    {
        let target_path = target_path.as_ref();
        // Create directory if it doesn't exist
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).context("Failed to create model directory")?;
        }

        // Construct download URL
        let url = format!(
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{}.bin",
            model_size
        );

        tracing::info!("Downloading from: {}", url);
        tracing::info!("Saving to: {}", target_path.display());

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

    /// Transcribe audio and return the segments with their end times.
    ///
    /// Audio should be mono, 16 kHz, f32. `prompt` conditions the decode on
    /// previous text. When `limit_audio_ctx` is set, the encoder context is
    /// scaled to the audio length, which is faster for short clips.
    pub fn transcribe_segments(
        &self,
        audio_data: &[f32],
        prompt: Option<&str>,
        limit_audio_ctx: bool,
    ) -> Result<Vec<TranscribedSegment>> {
        if audio_data.is_empty() {
            return Ok(Vec::new());
        }

        let mut state = self.state.lock();

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });

        if let Some(language) = self.language.as_deref() {
            params.set_language(Some(language));
        }

        params.set_n_threads(self.n_threads);
        // Each utterance is decoded independently; do not carry text context
        // from a previous call into this one.
        params.set_no_context(true);
        // Suppress non-speech tokens, which reduces hallucination on noise.
        params.set_suppress_nst(true);

        if let Some(prompt) = prompt.filter(|p| !p.is_empty()) {
            params.set_initial_prompt(prompt);
        }

        if limit_audio_ctx {
            params.set_audio_ctx(audio_ctx_for_samples(audio_data.len()));
        }

        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        state
            .full(params, audio_data)
            .context("Failed to transcribe audio")?;

        let num_segments = state.full_n_segments();
        let mut segments = Vec::with_capacity(num_segments.max(0) as usize);
        for i in 0..num_segments {
            if let Some(segment) = state.get_segment(i)
                && let Ok(segment_text) = segment.to_str_lossy()
            {
                segments.push(TranscribedSegment {
                    end_cs: segment.end_timestamp(),
                    text: segment_text.into_owned(),
                });
            }
        }

        Ok(segments)
    }

    /// Transcribe audio data and return the full text.
    ///
    /// Audio should be mono, 16kHz sample rate, f32 format
    pub fn transcribe(&self, audio_data: &[f32]) -> Result<String> {
        let segments = self.transcribe_segments(audio_data, None, false)?;
        let text: String = segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect();
        Ok(text.trim().to_string())
    }

    /// Run discarded transcriptions so the first real dictation does not pay
    /// for GPU graph compilation and buffer allocation.
    pub fn warm_up(&self) {
        // The unconstrained graph, used by the final transcriptions. This also
        // compiles the largest encoder-context bucket.
        let _ = self.transcribe_segments(&vec![0.0f32; 16_000], None, false);

        // The smaller encoder-context buckets used by live previews. Pass just
        // enough audio to select each bucket: the encoder processes the full
        // bucket-sized graph either way, so this bounds the wasted work.
        for samples in [1_600usize, 256 * 320 + 1_600, 512 * 320 + 1_600] {
            let _ = self.transcribe_segments(&vec![0.0f32; samples], None, true);
        }

        tracing::info!("Whisper warm-up complete");
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

    #[test]
    fn test_audio_ctx_bucket_selection() {
        assert_eq!(audio_ctx_for_samples(1_000), 256);
        assert_eq!(audio_ctx_for_samples(6 * 16_000), 512);
        assert_eq!(audio_ctx_for_samples(15 * 16_000), 1024);
        assert_eq!(audio_ctx_for_samples(28 * 16_000), 1500);
    }

    #[test]
    fn test_audio_ctx_covers_supported_audio() {
        // Every supported tail length must fit inside its bucket; a bucket that
        // is too small would truncate the audio.
        for seconds in [0.5f64, 2.0, 5.0, 10.0, 15.0, 20.0, 28.0] {
            let samples = (seconds * 16_000.0) as usize;
            let context = audio_ctx_for_samples(samples);
            assert!(
                context as usize * 320 >= samples,
                "context {context} too small for {seconds}s ({samples} samples)"
            );
        }
    }
}
