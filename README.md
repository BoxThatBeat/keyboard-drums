# keyboard-drums

Ultra-low latency drum sampler that turns your keyboard into a drum pad. Reads raw key events via Linux evdev and plays WAV samples through ALSA/PipeWire with minimal latency.

## How it works

keyboard-drums runs two threads connected by a lock-free ring buffer:

1. **Input thread** -- reads key-down events directly from `/dev/input/event*` via evdev (bypassing the compositor/terminal entirely)
2. **Audio thread** -- mixes triggered samples into a 48kHz stereo output stream via cpal

All samples are preloaded into memory at startup. The audio callback does zero heap allocations. This keeps trigger-to-sound latency as low as the audio buffer allows (typically 1-5ms).

## Requirements

- Linux (uses evdev for input, ALSA or PipeWire for audio)
- Rust toolchain (1.85+, edition 2024)
- ALSA development libraries: `sudo apt install libasound2-dev` (Debian/Ubuntu) or `sudo dnf install alsa-lib-devel` (Fedora)
- WAV sample files at 48kHz, mono or stereo

## Building

```sh
cargo build --release
```

The binary is at `target/release/keyboard-drums`.

To install it to `~/.cargo/bin`:

```sh
cargo install --path .
```

## Quick start

### 1. Find your keyboard device

```sh
keyboard-drums --list-devices
```

This prints all available evdev input devices. Look for your keyboard -- it's usually something like `/dev/input/event3`.

### 2. Set up permissions

By default, `/dev/input/event*` devices require root access. To run as a normal user, add yourself to the `input` group:

```sh
sudo usermod -aG input $USER
```

Then install the udev rule to grant the `input` group read access:

```sh
sudo cp udev/99-keyboard-drums.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger
```

Log out and back in for the group change to take effect.

### 3. Prepare samples

Create a directory for your WAV samples:

```sh
mkdir -p ~/.config/keyboard-drums/samples
```

Copy your `.wav` files there. Samples **must be 48kHz**. Mono and stereo are both supported. 16-bit, 24-bit integer, and 32-bit float formats all work.

If your samples are a different sample rate, convert them with ffmpeg:

```sh
ffmpeg -i kick_44100.wav -ar 48000 ~/.config/keyboard-drums/samples/kick.wav
```

### 4. Create a config file

```sh
mkdir -p ~/.config/keyboard-drums
cp config.example.toml ~/.config/keyboard-drums/config.toml
```

Edit the config to point to your device and samples:

```toml
device = "/dev/input/event3"
master_volume = 0.8
max_voices = 32
samples_dir = "/home/youruser/.config/keyboard-drums/samples"

[[bindings]]
key = "KEY_A"
sample = "kick.wav"
gain = 1.0

[[bindings]]
key = "KEY_S"
sample = "snare.wav"
gain = 0.9

[[bindings]]
key = "KEY_D"
sample = "hihat_closed.wav"
gain = 0.7
```

### 5. Run it

```sh
keyboard-drums
```

Press the bound keys to play samples. Stop with `Ctrl+C`.

## Usage

```
keyboard-drums [OPTIONS]

Options:
  -c, --config <CONFIG>  Path to config file [default: ~/.config/keyboard-drums/config.toml]
  -d, --device <DEVICE>  Override the evdev device path from config
      --list-devices     List available input devices and exit
  -v, --verbose          Enable verbose (debug) logging
  -h, --help             Print help
  -V, --version          Print version
```

Examples:

```sh
# Use a specific config file
keyboard-drums --config /path/to/config.toml

# Override the device from the command line
keyboard-drums --device /dev/input/event5

# Debug logging to see every keypress and trigger
keyboard-drums --verbose
```

## Configuration

The config file is TOML. See `config.example.toml` for a fully documented example.

| Field           | Type     | Default                                   | Description                                    |
|-----------------|----------|-------------------------------------------|------------------------------------------------|
| `device`        | string   | *(none)*                                  | Path to evdev device (e.g. `/dev/input/event3`) |
| `master_volume` | float    | `0.8`                                     | Global volume multiplier (0.0 to 1.0)          |
| `max_voices`    | integer  | `32`                                      | Max simultaneous sounds (oldest is stolen)      |
| `samples_dir`   | string   | *(required)*                              | Directory containing WAV files                  |
| `bindings`      | array    | *(required)*                              | Key-to-sample mappings (see below)              |

Each `[[bindings]]` entry has:

| Field    | Type   | Default | Description                                    |
|----------|--------|---------|------------------------------------------------|
| `key`    | string | *(required)* | Linux evdev key name (e.g. `KEY_A`, `KEY_SPACE`) |
| `sample` | string | *(required)* | WAV filename relative to `samples_dir`          |
| `gain`   | float  | `1.0`   | Per-sample volume (0.0 to 1.0)                  |

### Key names

Key names follow the Linux input event code naming convention. Common examples:

| Key name         | Key         |
|------------------|-------------|
| `KEY_A` - `KEY_Z` | Letter keys |
| `KEY_1` - `KEY_0` | Number row  |
| `KEY_SPACE`      | Space bar   |
| `KEY_SEMICOLON`  | `;`         |
| `KEY_COMMA`      | `,`         |
| `KEY_DOT`        | `.`         |
| `KEY_SLASH`      | `/`         |
| `KEY_LEFTSHIFT`  | Left Shift  |
| `KEY_LEFTCTRL`   | Left Ctrl   |

Run with `--verbose` to see the key codes for any key you press.

## Voice stealing

When the number of simultaneously playing samples exceeds `max_voices`, the oldest voices are silently removed to make room for new ones. This prevents audio glitches from too many overlapping sounds.

## Running as a service

A systemd user service is included for running keyboard-drums in the background:

```sh
mkdir -p ~/.config/systemd/user
cp systemd/keyboard-drums.service ~/.config/systemd/user/

systemctl --user daemon-reload
systemctl --user enable keyboard-drums
systemctl --user start keyboard-drums
```

View logs:

```sh
journalctl --user -u keyboard-drums -f
```

Stop:

```sh
systemctl --user stop keyboard-drums
```

## License

MIT
