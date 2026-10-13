use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
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

/// Hugging Face repository the GGML models are downloaded from.
///
/// Point this at a fork or mirror to control the supply chain. The published
/// SHA-256 is checked against this repository's metadata after download.
const MODEL_REPO: &str = "ggerganov/whisper.cpp";

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

/// Preferred directory for Whisper models (`$XDG_DATA_HOME/whisper`).
fn model_dir() -> Result<PathBuf> {
    match xdg::BaseDirectories::new().get_data_home() {
        Some(data_home) => Ok(data_home.join("whisper")),
        None => Ok(
            PathBuf::from(std::env::var("HOME").context("HOME is not set")?)
                .join(".local/share/whisper"),
        ),
    }
}

/// Locations used by earlier versions, still searched for existing models.
fn legacy_model_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(data_home) = xdg::BaseDirectories::new().get_data_home()
        && let Some(parent) = data_home.parent()
    {
        dirs.push(parent.join("whisper"));
    }
    if let Ok(home) = std::env::var("HOME") {
        let legacy = PathBuf::from(home).join(".local/whisper");
        if !dirs.contains(&legacy) {
            dirs.push(legacy);
        }
    }
    dirs
}

/// Map a user-facing model name to the upstream GGML file stem.
///
/// `large` is an alias for the latest large model, `large-v3`, and `turbo` maps
/// to `large-v3-turbo`. Other names pass through unchanged, so any tier that
/// exists upstream can be selected.
fn model_file_stem(model_name: &str) -> String {
    match model_name {
        "large" => "large-v3".to_owned(),
        "turbo" => "large-v3-turbo".to_owned(),
        other => other.to_owned(),
    }
}

/// Stream `reader` into `path`, updating `progress`, and return the SHA-256 of
/// the bytes written.
fn stream_to_file<R>(mut reader: R, path: &Path, progress: &ProgressBar) -> Result<Vec<u8>>
where
    R: io::Read,
{
    let file = fs::File::create(path)
        .with_context(|| format!("Failed to create file: {}", path.display()))?;

    struct HashingWriter<W: Write> {
        writer: W,
        hasher: Sha256,
        progress_bar: ProgressBar,
    }

    impl<W: Write> Write for HashingWriter<W> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let written = self.writer.write(buf)?;
            self.hasher.update(&buf[..written]);
            self.progress_bar.inc(written as u64);
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.writer.flush()
        }
    }

    let mut writer = HashingWriter {
        writer: file,
        hasher: Sha256::new(),
        progress_bar: progress.clone(),
    };

    io::copy(&mut reader, &mut writer).context("Failed to write response to file")?;
    Ok(writer.hasher.finalize().to_vec())
}

/// Model metadata published by HuggingFace.
struct ExpectedChecksum {
    size: u64,
    sha256: String,
}

/// Fetch the published size and SHA-256 for a model from the HuggingFace API.
/// Returns `None` if the metadata cannot be retrieved.
fn expected_model_checksum(stem: &str) -> Option<ExpectedChecksum> {
    let file_name = format!("ggml-{stem}.bin");
    let url = format!("https://huggingface.co/api/models/{MODEL_REPO}?blobs=true");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;
    let response = client.get(url).send().ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = response.text().ok()?;
    let json: serde_json::Value = serde_json::from_str(&body).ok()?;
    for sibling in json.get("siblings")?.as_array()? {
        if sibling.get("rfilename").and_then(|value| value.as_str()) != Some(file_name.as_str()) {
            continue;
        }
        let lfs = sibling.get("lfs")?;
        let sha256 = lfs
            .get("sha256")
            .and_then(|value| value.as_str())?
            .to_owned();
        let size = lfs
            .get("size")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        return Some(ExpectedChecksum { size, sha256 });
    }
    None
}

/// Decode a hex string into bytes.
fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

impl Transcriber {
    /// Create a new transcriber with the specified model
    ///
    /// Model files should be in GGML format and located at:
    /// ~/.local/share/whisper/ggml-<model>.bin
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

    /// Get the path to the Whisper model file, downloading if necessary.
    ///
    /// Models live in the XDG data directory (`$XDG_DATA_HOME/whisper`). A
    /// legacy location (`~/.local/whisper`) and the current directory are also
    /// searched, so existing installations keep working.
    fn get_model_path(model_size: &str) -> Result<PathBuf> {
        let stem = model_file_stem(model_size);
        let file_name = format!("ggml-{stem}.bin");

        // Preferred location, and where a missing model is downloaded to.
        let model_path = model_dir()?.join(&file_name);
        if model_path.exists() {
            return Ok(model_path);
        }

        for dir in legacy_model_dirs() {
            let candidate = dir.join(&file_name);
            if candidate.exists() {
                tracing::info!(path = %candidate.display(), "Using model from legacy location");
                return Ok(candidate);
            }
        }

        let local_model_path = PathBuf::from(&file_name);
        if local_model_path.exists() {
            return Ok(local_model_path);
        }

        tracing::info!("Model not found, downloading {file_name}...");
        Self::download_model(&stem, &model_path)?;
        Ok(model_path)
    }

    /// Download a Whisper model from HuggingFace.
    ///
    /// The download is written to a temporary file, checked for completeness
    /// against the advertised length, and only then moved into place. An
    /// interrupted download is therefore never mistaken for a complete model.
    fn download_model<P>(stem: &str, target_path: P) -> Result<()>
    where
        P: AsRef<Path>,
    {
        let target_path = target_path.as_ref();
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).context("Failed to create model directory")?;
        }

        let url = format!("https://huggingface.co/{MODEL_REPO}/resolve/main/ggml-{stem}.bin");
        tracing::info!(url, path = %target_path.display(), "Downloading Whisper model");

        let response = reqwest::blocking::get(&url)
            .with_context(|| format!("Failed to download model from {url}"))?;

        if !response.status().is_success() {
            anyhow::bail!(
                "Failed to download model: HTTP {}. Model '{stem}' may not exist.",
                response.status()
            );
        }

        let content_length = response
            .content_length()
            .context("Failed to get content length")?;

        // Published size and checksum, when the metadata can be reached.
        let expected = expected_model_checksum(stem);
        let expected_size = expected
            .as_ref()
            .map(|expected| expected.size)
            .filter(|size| *size > 0)
            .unwrap_or(content_length);

        let progress = ProgressBar::new(content_length);
        progress.set_style(
            ProgressStyle::default_bar()
                .template("{msg}\n{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})")
                .context("Invalid progress template")?
                .progress_chars("#>-"),
        );
        progress.set_message(format!("Downloading ggml-{stem}.bin"));

        let temp_path = target_path.with_extension("bin.part");

        let digest = match stream_to_file(response, &temp_path, &progress) {
            Ok(digest) => digest,
            Err(e) => {
                let _ = fs::remove_file(&temp_path);
                progress.finish_and_clear();
                return Err(e);
            }
        };

        let written = fs::metadata(&temp_path).map(|meta| meta.len()).unwrap_or(0);
        if written != expected_size {
            let _ = fs::remove_file(&temp_path);
            progress.finish_and_clear();
            anyhow::bail!("Incomplete model download: got {written} of {expected_size} bytes");
        }

        if let Some(expected) = &expected
            && let Some(expected_digest) = hex_decode(&expected.sha256)
            && expected_digest != digest
        {
            let _ = fs::remove_file(&temp_path);
            progress.finish_and_clear();
            anyhow::bail!("Checksum mismatch for ggml-{stem}.bin");
        }

        fs::rename(&temp_path, target_path).with_context(|| {
            format!("Failed to move model into place: {}", target_path.display())
        })?;
        progress.finish_with_message(format!("Downloaded ggml-{stem}.bin successfully!"));

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

    #[test]
    fn test_model_dir_ends_with_whisper() {
        let dir = model_dir().expect("model_dir");
        assert!(dir.ends_with("whisper"), "{}", dir.display());
    }

    #[test]
    fn test_hex_decode() {
        assert_eq!(hex_decode("00ff10"), Some(vec![0x00, 0xff, 0x10]));
        assert_eq!(hex_decode(""), Some(Vec::new()));
        assert_eq!(hex_decode("abc"), None, "odd length");
        assert_eq!(hex_decode("zz"), None, "invalid digits");
    }

    #[test]
    fn test_model_file_stem() {
        assert_eq!(model_file_stem("base"), "base");
        assert_eq!(model_file_stem("large"), "large-v3", "large is an alias");
        assert_eq!(model_file_stem("turbo"), "large-v3-turbo");
        assert_eq!(model_file_stem("large-v2"), "large-v2", "passes through");
    }
}
