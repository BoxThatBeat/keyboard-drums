# keyboard-drums

Ultra-low latency drum sampler that turns your keyboard into a drum pad. Reads raw key events via Linux evdev and plays WAV samples through ALSA/PipeWire with minimal latency.

## How it works

keyboard-drums runs two threads connected by a lock-free ring buffer:

1. **Input thread** -- reads key-down events directly from `/dev/input/event*` via evdev (bypassing the compositor/terminal entirely)
2. **Audio thread** -- mixes triggered samples into a 48kHz stereo output stream via cpal

All samples are preloaded into memory at startup. The audio callback does zero heap allocations. This keeps trigger-to-sound latency as low as the audio buffer allows (typically 1-5ms).

Samples are organized into **drum kits** with **variants**. You can cycle through kits and variants at runtime using configurable keybindings -- samples are swapped atomically so there is no interruption to audio.

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

Two devices need to be accessible: `/dev/input/event*` (to read keyboard events) and `/dev/uinput` (to create a virtual keyboard that forwards non-drum keys to other applications).

**Add yourself to the required groups:**

```sh
sudo usermod -aG input $USER
sudo groupadd -f uinput
sudo usermod -aG uinput $USER
```

**Install the udev rules:**

```sh
sudo cp udev/99-keyboard-drums.rules /etc/udev/rules.d/
echo 'KERNEL=="uinput", MODE="0660", GROUP="uinput"' | sudo tee /etc/udev/rules.d/99-uinput.rules
sudo udevadm control --reload-rules
sudo udevadm trigger
```

**Ensure the uinput kernel module is loaded:**

```sh
sudo modprobe uinput
```

To load it automatically on boot:

```sh
echo uinput | sudo tee /etc/modules-load.d/uinput.conf
```

Log out and back in for the group changes to take effect.

### 3. Prepare samples

Create the samples directory structure. Samples are organized into **kits** and **variants**:

```
~/.config/keyboard-drums/samples/
  acoustic/
    variant1/
      kick.wav
      snare.wav
      hihat.wav
    variant2/
      kick.wav
      snare.wav
      hihat.wav
  electronic/
    variant1/
      kick.wav
      snare.wav
      hihat.wav
```

Every variant folder within a kit must contain the same set of WAV files (matching the filenames in your bindings config). The first kit (alphabetically) and first variant are loaded on startup.

Samples **must be 48kHz**. Mono and stereo are both supported. 16-bit, 24-bit integer, and 32-bit float formats all work.

If your samples are a different sample rate, convert them with ffmpeg:

```sh
ffmpeg -i kick_44100.wav -ar 48000 kick.wav
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

[cycling_keys]
next_kit = "KEY_RIGHT"
prev_kit = "KEY_LEFT"
next_variant = "KEY_UP"
prev_variant = "KEY_DOWN"

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
| `samples_dir`   | string   | *(required)*                              | Root directory containing kit folders            |
| `bindings`      | array    | *(required)*                              | Key-to-sample mappings (see below)              |
| `cycling_keys`  | table    | *(all empty)*                             | Keys for cycling kits/variants (see below)      |

Each `[[bindings]]` entry has:

| Field    | Type   | Default | Description                                    |
|----------|--------|---------|------------------------------------------------|
| `key`    | string | *(required)* | Linux evdev key name (e.g. `KEY_A`, `KEY_SPACE`) |
| `sample` | string | *(required)* | WAV filename present in every variant folder    |
| `gain`   | float  | `1.0`   | Per-sample volume (0.0 to 1.0)                  |

The `[cycling_keys]` table (all fields optional):

| Field           | Type   | Default | Description                                |
|-----------------|--------|---------|--------------------------------------------|
| `next_kit`      | string | *(none)* | Key to cycle forward through drum kits     |
| `prev_kit`      | string | *(none)* | Key to cycle backward through drum kits    |
| `next_variant`  | string | *(none)* | Key to cycle forward through variants      |
| `prev_variant`  | string | *(none)* | Key to cycle backward through variants     |

Cycling keys must not conflict with sample keybindings. When switching kits, the variant resets to the first one. Cycling wraps around in both directions.

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

## Drum kits and variants

Samples are organized into a two-level directory structure under `samples_dir`:

```
samples_dir/<kit>/<variant>/<sample>.wav
```

- **Kits** are the top-level folders (e.g. `acoustic`, `electronic`). Sorted alphabetically.
- **Variants** are subfolders within each kit (e.g. `variant1`, `variant2`). Sorted alphabetically.
- Each variant must contain all WAV files referenced in `[[bindings]]`.
- Variants missing any required sample are skipped with a warning.
- Kits with no valid variants are skipped entirely.

At startup, the first kit and first variant are loaded. Press the configured cycling keys to switch at runtime. The sample swap is atomic -- any currently playing voices will finish with their original samples while new triggers use the new ones.

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
