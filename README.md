# sw1nn-transcription

Real-time voice transcription with Whisper.

## Installation

### Arch Linux

Install the package:
```bash
makepkg -si
```

Enable and start the systemd user service:
```bash
systemctl --user enable --now sw1nn-transcription.service
```

Check service status:
```bash
systemctl --user status sw1nn-transcription.service
```

## Configuration

Copy the sample configuration:
```bash
mkdir -p ${XDG_CONFIG_HOME:-~/.config}/stentor
cp /usr/share/doc/sw1nn-transcription/config.toml.sample ${XDG_CONFIG_HOME:-~/.config}/stentor/config.toml
```

Edit `${XDG_CONFIG_HOME}/stentor/config.toml` to customize settings.

## Usage

Once the daemon is running, trigger transcription:
```bash
stentorctl transcribe
```

The daemon will:
1. Open a GTK dialog showing audio levels
2. Record your speech with voice activity detection
3. Transcribe using Whisper
4. Display the transcribed text for review
5. Execute the configured output command when you press Ctrl+Enter

### Example Output Commands

In your `config.toml`:

```toml
[daemon]
# Copy to clipboard (Wayland)
output-command = "wl-copy"

# Or send to a tmux session
output-command = "tmux send-keys -t claude -l \"$TRANSCRIPTION\" && tmux send-keys -t claude Enter"

# Or paste directly with xdotool
output-command = "xdotool type --clearmodifiers \"$TRANSCRIPTION\""
```

The transcribed text is available via the `$TRANSCRIPTION` environment variable.

## License

GPL-3.0
