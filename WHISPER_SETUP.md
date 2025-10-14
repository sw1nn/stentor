# Whisper Model Setup for Rust Version

The Rust implementation uses [whisper-rs](https://github.com/tazz4843/whisper-rs), which provides Rust bindings to [whisper.cpp](https://github.com/ggerganov/whisper.cpp).

## Automatic Model Download

**Models are downloaded automatically on first use!** When you start the daemon with a model that doesn't exist locally, it will be downloaded from HuggingFace automatically.

No manual setup required - just configure your preferred model and the daemon will handle the rest.

## Model Files

Models are stored in GGML format at:
```
~/.local/share/whisper/ggml-{model_size}.bin
```

## Available Models

- `ggml-tiny.bin` - Fastest, ~75 MB
- `ggml-base.bin` - Good balance, ~142 MB (default)
- `ggml-small.bin` - Better accuracy, ~466 MB
- `ggml-medium.bin` - High accuracy, ~1.5 GB
- `ggml-large.bin` - Best accuracy, ~2.9 GB

## Manual Download (Optional)

If you prefer to download models manually (e.g., for offline use):
```bash
mkdir -p ~/.local/share/whisper
cd ~/.local/share/whisper

# Download tiny model (fastest)
wget https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin

# Download base model (default)
wget https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin

# Download small model
wget https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin
```

Manual downloads are stored in the same location as automatic downloads, so the daemon will detect them.

## Configuration

Set the model in your config file (`~/.config/sw1nn-transcription/config.toml`):
```toml
model = "base"  # or "tiny", "small", "medium", "large"
language = "en"  # or "auto" for automatic detection
```

## Performance Notes

- **tiny**: Very fast, suitable for real-time use, good accuracy for clear speech
- **base**: Recommended default, good balance of speed and accuracy
- **small**: Slower but more accurate, may have latency
- **medium/large**: Best accuracy but slow, not recommended for real-time use

## Technical Details

- Input format: Mono, 16kHz, f32 samples
- The audio module automatically handles resampling to 16kHz
- Whisper processes audio in ~30 second chunks internally
