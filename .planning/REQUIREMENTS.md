# keyboard-drums Requirements

## Core Concept

A console application written in Rust that plays ultra-low latency drum samples when
pre-configured keyboard keys are pressed, even when the application is not in focus.
Designed for tapping along to music using a keyboard as a drum pad.

## Functional Requirements

### Audio Playback
- Play WAV drum samples at ultra-low latency (<10ms end-to-end target)
- Support high polyphony: multiple samples playing simultaneously (stacking)
- Same-sample retrigger behavior: stack (polyphonic) — overlapping instances mix together
- Global voice limit (default 32) with oldest-voice stealing when limit is reached
- Output at 48kHz sample rate
- All samples preloaded into memory at startup (zero disk I/O during playback)

### Volume Control
- Global master volume (0.0-1.0) in config
- Per-sample gain adjustment (0.0-1.0) in config
- Final output clamped to [-1.0, 1.0] to prevent clipping

### Input Handling
- Read keyboard input directly from `/dev/input/eventX` using evdev
- Passive listening — key events still pass through to other applications
- Respond to key-down events only (designed for future key-up support)
- Support selecting keyboard device by CLI argument or config file
- Must work system-wide (background mode) without window focus

### Configuration
- TOML config file format
- Config specifies: device path, master volume, max voices, samples directory, keybindings
- Each keybinding maps an evdev key name (e.g. `KEY_A`) to a sample file and gain
- Default config location: `~/.config/keyboard-drums/config.toml`
- Config path overridable via CLI argument

### Runtime Control
- `SIGTERM` / `SIGINT`: graceful shutdown
- `SIGHUP`: reload config and samples without restart

### CLI Interface
- `--config <path>` — specify config file (default: `~/.config/keyboard-drums/config.toml`)
- `--device <path>` — override evdev device path from config
- `--list-devices` — print available evdev input devices and exit
- `--verbose` / `-v` — increase log verbosity

## Non-Functional Requirements

### Latency
- **Utmost importance** — drives all architectural decisions
- Lock-free communication between input and audio threads
- No heap allocations in the audio callback hot path
- Request smallest available audio buffer size from the audio backend
- Direct kernel evdev input — no X11/Wayland compositor delay
- Target latency budget:
  - evdev kernel to userspace: ~0.1ms
  - Ring buffer push/pop: ~0.001ms
  - Audio buffer (64 frames @ 48kHz): ~1.3ms
  - ALSA/PipeWire output: ~2-5ms
  - **Total: ~3.5-6.5ms**

### Deployment
- Run as a systemd user service
- udev rule for granting read access to `/dev/input/event*` devices
- Linux-only (ALSA or PipeWire backend)

### Logging
- Structured logging with timestamps and module paths
- Key events logged: config loaded, samples loaded (with durations), device opened,
  triggers sent, voices spawned/stolen, shutdown/reload
- Logging is for development debugging and can be removed once stable

## Audio Format Support
- WAV files only (16-bit or 24-bit)
- Mono or stereo
- Reject samples that don't match the configured sample rate (48kHz) rather than resampling

## Constraints
- Rust programming language
- Linux-only target
- No GUI required
- Small kit size (4-8 samples typical, no hard limit)
