# Stentor

Real-time voice transcription daemon using OpenAI's Whisper model. Stentor provides low-latency speech-to-text with a GTK4 interface, voice activity detection, and flexible output options including multi-window support for Kitty terminal.

## Features

- **Real-time transcription** with periodic preview updates while speaking
- **Voice Activity Detection (VAD)** automatically starts/stops recording based on speech
- **Confirmed/preview text display** - stable text in white, tentative text in grey
- **Multiple Whisper models** - tiny, base, small, medium, large, large-v3-turbo
- **PulseAudio integration** with configurable audio source
- **Multi-slot output** - send transcriptions to multiple Kitty terminal windows
- **D-Bus control interface** - drive the daemon from any D-Bus client
- **Configurable output commands** - clipboard, tmux, xdotool, or custom scripts

## Installation

### Arch Linux

Build and install the package:

```bash
cd packaging/arch
makepkg -si
```

Enable and start the systemd user service:

```bash
systemctl --user enable --now stentor.service
```

Check service status:

```bash
systemctl --user status stentor.service
```

### From Source

Requirements:
- Rust (edition 2024)
- GTK4 and libadwaita
- PulseAudio development libraries
- whisper.cpp (via whisper-rs)

```bash
cargo build --release
```

Note: Release builds are recommended for best performance, though GPU acceleration reduces the difference.

#### Build Features

| Feature | Description |
|---------|-------------|
| `whisper-tracing-backend` | Route whisper.cpp logs through tracing for debugging GPU initialization |

Example:

```bash
cargo build --release --features whisper-tracing-backend
```

## Configuration

Create the configuration directory and copy the sample config:

```bash
mkdir -p ${XDG_CONFIG_HOME:-~/.config}/stentor
cp config.toml.sample ${XDG_CONFIG_HOME:-~/.config}/stentor/config.toml
```

### Daemon Configuration `[daemon]`

| Option | Default | Description |
|--------|---------|-------------|
| `model` | `"base"` | Whisper model: tiny, base, small, medium, large (= large-v3), large-v1/v2/v3, large-v3-turbo |
| `language` | `"en"` | ISO 639-1 language code, or "auto" for detection |
| `silence-duration` | `2.0` | Seconds of silence before stopping recording |
| `silence-threshold` | `0.002` | RMS threshold for silence detection (lower = more sensitive) |
| `ptt-tap-threshold` | `0.3` | Push-to-talk tap threshold in seconds; `0` disables tap-to-latch |
| `min-speech-duration` | `0.5` | Minimum speech duration before recording starts |
| `chunk-size` | `1024` | Audio chunk size in samples (64ms at 16kHz) |
| `periodic-transcription-interval` | `1.0` | How often to transcribe while speaking (seconds) |
| `transcription-window` | `28.0` | Maximum audio tail decoded in one transcription pass (seconds) |
| `transcription-lag` | `1.0` | Delay before audio is confirmed (seconds) |
| `output-command` | none | Command to execute with transcribed text |

### Client Configuration `[client]`

| Option | Default | Description |
|--------|---------|-------------|
| `source` | none | Default audio source (microphone) |
| `multi-slot-handler` | none | Multi-slot handler: `"none"` or `"kitty"` |

### Kitty Configuration `[kitty]`

| Option | Default | Description |
|--------|---------|-------------|
| `background-color-cmd` | none | Command to retrieve current background color |
| `base-background-color` | `"#1e1e2e"` | Fallback background color |
| `output-command` | none | Kitty-specific output command template |
| `slot-matches` | `[]` | Patterns for auto-discovering windows (see below) |
| `slot-id-font` | `"monospace:bold"` | Font for slot number background images |

### Example Configuration

```toml
[daemon]
model = "base"
language = "en"
silence-duration = 2.0
silence-threshold = 0.002
output-command = "wl-copy"

[client]
source = "alsa_input.usb-Blue_Microphones"
multi-slot-handler = "kitty"

[kitty]
base-background-color = "#1e1e2e"
# Auto-discover windows running claude or nvim
slot-matches = ["claude", "nvim"]
```

## Usage

### Basic Commands

```bash
# Start recording (opens dialog and begins listening)
stentorctl transcribe

# Start recording with source unmute
stentorctl transcribe --unmute-source

# Start recording with specific microphone
stentorctl transcribe --source="USB Condenser Microphone"

# Start recording with Kitty multi-slot handler
stentorctl transcribe --multi-slot-handler=kitty

# Stop recording and auto-send to the focused kitty window (kitty mode), else default output
stentorctl transcribe-end

# Stop recording and auto-send to slot 1
stentorctl transcribe-end --slot=1

# Stop and send to default output (slot 0)
stentorctl transcribe-end --slot=0

# Push-to-talk: press starts recording, release stops and sends
stentorctl ptt-down --unmute-source
stentorctl ptt-up

# List available audio sources
stentorctl list-sources

# Check daemon status
stentorctl status

# Quit daemon
stentorctl quit
```

### D-Bus Control Interface

The daemon exposes its control operations on the user session bus. `stentorctl`
is a client for this interface, but any D-Bus client can call it directly.

| Property | Value |
|----------|-------|
| Bus name | `com.sw1nn.stentor` |
| Object path | `/com/sw1nn/stentor` |
| Interface | `com.sw1nn.stentor.Control` |

Methods:

| Method | Signature | Description |
|--------|-----------|-------------|
| `Start` | `bssb` | Start recording. Arguments: unmute source, audio source, multi-slot handler (`none` or `kitty`), push-to-talk. An empty source uses the configured or default source. |
| `Stop` | — | Stop recording and send to the focused destination, all active destinations, or the default output. |
| `StopAtSlot` | `u` | Stop recording and send to a specific slot. |
| `PttUp` | — | Release the push-to-talk key. |
| `Status` | — | Return `running` when the daemon is available. |
| `Quit` | — | Ask the daemon to exit. |

Inspect or call the interface with `busctl`:

```bash
busctl --user introspect com.sw1nn.stentor /com/sw1nn/stentor
busctl --user call com.sw1nn.stentor /com/sw1nn/stentor com.sw1nn.stentor.Control Status
```

> [!NOTE]
> The former `socket-name` configuration key is accepted but ignored. The daemon
> logs a warning when it is present.
>
> Control requires a reachable user session bus. Only one daemon can own
> `com.sw1nn.stentor` at a time. A stale `$XDG_RUNTIME_DIR/stentor.sock` left by
> an older version can be deleted.

### Workflow

1. Run `stentorctl transcribe` to open the transcription dialog
2. The dialog shows "Listening..." while waiting for speech
3. When speech is detected, recording begins with real-time preview
4. Preview text appears in grey (tentative), confirmed text in white (stable)
5. After `silence-duration` seconds of silence, recording stops
6. Review and edit the transcription in the text area
7. Press a keyboard shortcut to send the text to your desired destination

### Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Escape` | Cancel and close dialog |
| `F2` | Toggle hold: review on end instead of sending |
| `Alt+Enter` | Send to all active destinations |
| `Alt+1` - `Alt+8` | Send to specific slot |
| `Alt+0` | Send to default output (slot 0) |

During recording, shortcuts stop recording and auto-send. During review, they send the edited text.

`F2` toggles "review on release" for the current session. It is useful with a
foot switch: the release normally sends the text, but with the toggle armed the
text stays in the dialog for review, so you can edit it or pick a different
destination. An explicit `transcribe-end --slot=N` always sends. The toggle
resets at the start of each recording.

### Push-to-Talk (Foot Pedal)

`ptt-down` and `ptt-up` are made for a momentary foot pedal. The daemon owns
the press timing, so a daemon restart cannot leave a stale latch behind.

- **hold longer than `ptt-tap-threshold`** — start on press, stop and send on
  release;
- **tap shorter than `ptt-tap-threshold`** — the session stays open (the
  microphone and any source unmute stay on) until the next `ptt-down`, which
  stops and sends.

Silence does not end a push-to-talk session; only a release, a second press, or
an explicit stop does. The transcript is auto-sent on this path unless
review-on-release is armed with F2. Use `transcribe` / `transcribe-end` for the
hands-free workflow with silence auto-stop and review.

Bind both edges of the pedal key. For Hyprland:

```lua
hl.bind(
    "XF86Tools",
    hl.dsp.exec_cmd("stentorctl ptt-down --unmute-source"),
    { description = "Push-to-talk down" }
)
hl.bind(
    "XF86Tools",
    hl.dsp.exec_cmd("stentorctl ptt-up"),
    { release = true, description = "Push-to-talk up" }
)
```

To make every release stop and send, set `ptt-tap-threshold = 0` in the
`[daemon]` section.

## Multi-Slot Mode (Kitty Terminal)

Stentor can send transcriptions to multiple Kitty terminal windows simultaneously. Each window is assigned a colored slot (1-8) for visual identification with a large slot number displayed in the top-right corner.

### Window Discovery

Windows can be discovered in two ways:

#### 1. Automatic Discovery (Recommended)

Configure `slot-matches` to automatically discover windows based on the foreground process:

```toml
[kitty]
# Match windows running claude or nvim
slot-matches = ["claude", "nvim"]

# With explicit prefix (equivalent)
slot-matches = ["cmd:claude", "cmd:nvim"]
```

Windows are auto-assigned slot numbers 1-8 in order of discovery (sorted by window ID).

#### 2. Explicit Slot Assignment

Launch windows with the `STENTOR_SLOT` environment variable for explicit slot assignment:

```bash
# Launch a new Kitty window with slot assignment
stentorctl launch zsh

# Or manually set the environment variable
STENTOR_SLOT=1 kitty
```

Explicit assignments take priority - auto-discovered windows skip occupied slots.

### Usage

1. Start transcription with the Kitty handler:

```bash
stentorctl transcribe --multi-slot-handler=kitty
```

2. Matching windows get a colored background with their slot number displayed
3. The dialog shows color-coded destination buttons for each discovered window
4. Press `Alt+1` through `Alt+8` to send to specific slots, or `Alt+Enter` for all

### How It Works

- Windows are discovered via Kitty's remote control API
- Each slot gets a unique background color from a Catppuccin-based palette
- A large slot number (1-8) is displayed in the top-right corner of each window
- Window labels show the git repository and branch name (if in a git worktree)
- Background images and colors reset when the dialog closes

## Output Commands

The transcribed text is available via the `$TRANSCRIPTION` environment variable. The slot number is available via `$SLOT`.

### Examples

```toml
[daemon]
# Copy to clipboard (Wayland)
output-command = "wl-copy"

# Copy to clipboard (X11)
output-command = "xclip -selection clipboard"

# Type directly with xdotool
output-command = "xdotool type --clearmodifiers \"$TRANSCRIPTION\""

# Send to tmux session
output-command = "tmux send-keys -t mysession -l \"$TRANSCRIPTION\""

# Send to tmux and press Enter
output-command = "tmux send-keys -t claude -l \"$TRANSCRIPTION\" && tmux send-keys -t claude Enter"

[kitty]
# Send text directly to kitty windows matching the slot
# KITTY_LISTEN_ON is automatically set by the kitty multi-slot handler
output-command = "kitty @ send-text --match=env:STENTOR_SLOT=$SLOT \"$TRANSCRIPTION\""
```

## Daemon Options

The daemon (`stentord`) accepts command-line overrides:

```bash
stentord --model large --language auto --silence-duration 3.0
```

| Option | Description |
|--------|-------------|
| `--model` | Override Whisper model |
| `--language` | Override language code |
| `--output-command` | Override output command |
| `--silence-duration` | Override silence duration |
| `-q, --quiet` | Disable logging output |

## Performance

### GPU Acceleration (Critical for Real-Time Use)

Stentor uses Whisper for speech recognition, which is computationally intensive. **GPU acceleration is essential for acceptable real-time performance.** Without it, transcription will be slow and laggy.

The default build enables Vulkan (GPU) and OpenMP (multi-threaded CPU) acceleration:

```toml
# In Cargo.toml
whisper-rs = { version = "0.15", features = ["vulkan", "openmp"] }
```

#### Performance Impact

Profiling shows the dramatic difference GPU offloading makes:

| Metric | CPU Only | Vulkan + OpenMP | Improvement |
|--------|----------|-----------------|-------------|
| CPU Samples | 18K | 1K | 18x fewer |
| CPU Cycles | 689 billion | 14.5 billion | **47x fewer** |
| Hotspot | `ggml_vec_dot_f16` (80%) | `ggml_vk_wait_for_fence` (54%) | GPU offload |

With GPU acceleration, the CPU spends most of its time waiting for the GPU rather than doing compute. The heavy matrix operations run on your graphics card.

#### Supported Backends

| Feature | Backend | Use Case |
|---------|---------|----------|
| `vulkan` | Vulkan API | AMD, Intel, NVIDIA (recommended for AMD) |
| `cuda` | NVIDIA CUDA | NVIDIA GPUs |
| `metal` | Apple Metal | macOS |
| `openblas` | OpenBLAS | Optimized CPU BLAS |
| `openmp` | OpenMP | Multi-threaded CPU |

#### Verifying GPU Acceleration

Check that Vulkan is linked:

```bash
# Should show libvulkan linked
ldd $(which stentord) | grep vulkan
```

To see detailed whisper.cpp initialization logs (including GPU backend info), build with the `whisper-tracing-backend` feature:

```bash
cargo build --release --features whisper-tracing-backend
```

## Troubleshooting

### Check logs

```bash
journalctl --user -u stentor.service -f
```

### List available microphones

```bash
stentorctl list-sources
```

### Microphone not detected

Ensure PulseAudio is running and the microphone is not muted:

```bash
pactl list sources short
```

When the selected source is a virtual microphone (for example EasyEffects), the
dialog also shows the physical device that feeds it, e.g. `USB Condenser
Microphone Mono → Easy Effects Source`. That lookup reads the PipeWire link
graph with `pw-dump`, which ships with PipeWire; on a PulseAudio-only system the
label falls back to the source description.

### Transcription too slow

First, ensure GPU acceleration is enabled (see [Performance](#performance) section). Without GPU offloading, transcription will be very slow.

If GPU is enabled and still slow, try a smaller model:

```toml
[daemon]
model = "tiny"  # fastest, least accurate
```

### Recording stops too quickly

Increase silence duration:

```toml
[daemon]
silence-duration = 3.0
```

### Not sensitive enough to quiet speech

Lower the silence threshold:

```toml
[daemon]
silence-threshold = 0.001
```

## License

GPL-3.0
