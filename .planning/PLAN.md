# keyboard-drums Implementation Plan

## Architecture Overview

```
+-----------------+     lock-free ringbuf      +----------------------+
| Thread 1: Input |  -- Trigger { sample_id, ->| Thread 2: Audio      |
| (evdev blocking |     velocity }             | (cpal callback)      |
|  read loop)     |     128-slot SPSC ring     |                      |
+-----------------+                            | - Mixes active       |
                                               |   voices into        |
                                               |   output buffer      |
                                               | - Voice stealing     |
                                               |   when limit hit     |
                                               +----------------------+
                                                        |
                                                  ALSA / PipeWire
                                                  48kHz output
```

## Project Structure

```
keyboard-drums/
  Cargo.toml
  config.example.toml
  udev/
    99-keyboard-drums.rules
  systemd/
    keyboard-drums.service
  src/
    main.rs          # CLI parsing, signal handling, orchestration
    config.rs        # TOML config parsing & validation
    input.rs         # evdev keyboard reader thread
    audio.rs         # cpal audio engine + voice mixer
    samples.rs       # WAV loading & preloading into memory
    ring.rs          # Lock-free ring buffer trigger channel
```

## Crate Dependencies

| Crate         | Purpose                                      |
|---------------|----------------------------------------------|
| evdev         | Read raw keyboard events from /dev/input     |
| hound         | Decode WAV files                             |
| ringbuf       | Lock-free SPSC ring buffer for triggers      |
| cpal          | Cross-platform audio output (ALSA on Linux)  |
| crossbeam     | Thread coordination (scoped threads)         |
| clap          | CLI argument parsing                         |
| serde + toml  | Config deserialization                       |
| log + env_logger | Structured logging                        |
| signal-hook   | Async-signal-safe SIGHUP/SIGTERM handling    |

## Core Data Structures

```rust
// Trigger message - fits in a cache line, no allocation
struct Trigger {
    sample_id: u8,    // index into preloaded sample array
    velocity: f32,    // 1.0 for now (key-down only), extensible
}

// Single playing voice
struct Voice {
    sample_id: u8,
    position: usize,  // playback cursor into sample data
    gain: f32,        // per-sample gain * master gain
}

// Preloaded sample data
struct SampleData {
    data: Vec<f32>,   // interleaved samples, normalized to [-1.0, 1.0]
    channels: u16,    // 1 (mono) or 2 (stereo)
    sample_rate: u32, // must match output rate (48kHz)
}
```

## Implementation Phases

### Phase 1: Project Scaffolding
1. Run `cargo init` to create the Rust project
2. Configure `Cargo.toml` with all dependencies and version pins
3. Create `config.example.toml` with documented example configuration
4. Initial commit

### Phase 2: Config System (config.rs)
5. Define config structs with serde Deserialize:
   - `Config`: device, master_volume, max_voices, samples_dir, bindings
   - `Binding`: key (evdev name string), sample (filename), gain
6. Implement config loading from TOML file with validation:
   - Check sample files exist
   - Validate key names are valid evdev keys
   - Clamp gain/volume to [0.0, 1.0]
7. Unit tests for config parsing
8. Commit

### Phase 3: Sample Loading (samples.rs)
9. Load WAV files via `hound` into `Vec<f32>` (normalized)
10. Handle mono and stereo formats
11. Reject samples that don't match 48kHz (error with helpful message)
12. Store as indexed `Vec<SampleData>`
13. Log sample metadata on load (duration, channels, sample rate, size)
14. Unit tests for WAV loading
15. Commit

### Phase 4: Ring Buffer (ring.rs)
16. Thin wrapper around `ringbuf::HeapRb<Trigger>` with 128 slots
17. Producer: push trigger, log warning if buffer full (dropped trigger)
18. Consumer: drain all available triggers
19. Unit tests for push/pop behavior
20. Commit

### Phase 5: Audio Engine (audio.rs)
21. Initialize cpal output stream at 48kHz
22. Request smallest available buffer size for lowest latency
23. Maintain pre-allocated `Vec<Voice>` (capacity = max_voices) in callback
24. Audio callback per iteration:
    a. Drain triggers from ring buffer -> spawn new voices
    b. If voice count > max_voices, steal oldest voice
    c. Mix all active voices into output buffer (additive with gain)
    d. Remove finished voices
    e. Apply master volume
    f. Clamp output to [-1.0, 1.0]
25. Zero heap allocations in the hot path
26. Commit

### Phase 6: Input Reader (input.rs)
27. Open evdev device by path
28. Implement `--list-devices` to enumerate available input devices
29. Blocking read loop: filter EV_KEY events with value=1 (key down)
30. Key code lookup via HashMap<u16, usize> (key code -> sample index)
31. On match: push Trigger to ring buffer producer
32. Debug logging for each keypress
33. Commit

### Phase 7: Main Orchestration (main.rs)
34. CLI parsing with clap:
    - `--config <path>` (default ~/.config/keyboard-drums/config.toml)
    - `--device <path>` (overrides config)
    - `--list-devices`
    - `--verbose` / `-v`
35. Startup sequence: load config -> preload samples -> create ring buffer
36. Signal handlers via signal-hook:
    - SIGTERM/SIGINT -> atomic shutdown flag
    - SIGHUP -> atomic reload flag
37. Spawn input thread with crossbeam scoped threads
38. Start audio stream on main thread
39. Main loop: check shutdown/reload flags
40. Graceful shutdown: stop audio, join input thread
41. Commit

### Phase 8: Deployment Files
42. Create udev rule: `99-keyboard-drums.rules`
    - Grant read access to /dev/input/event* for input group
43. Create systemd user service: `keyboard-drums.service`
    - Type=simple
    - ExecStart pointing to binary
    - ExecReload=kill -HUP $MAINPID
    - Restart policy
44. Commit

### Phase 9: Integration Testing & Polish
45. End-to-end manual testing with real keyboard device
46. Verify latency characteristics
47. Review all log points for usefulness
48. Final commit with any fixes

## Key Latency Decisions

1. **Lock-free ring buffer** - No mutex contention between input and audio threads
2. **Pre-loaded samples** - Zero disk I/O during playback
3. **Smallest buffer size** - Request minimum from cpal (typically 64-256 frames)
4. **No allocations in audio callback** - Pre-allocated voice array
5. **Direct evdev input** - Kernel events, no compositor delay
6. **f32 sample mixing** - Simple multiply-add, no format conversion in hot path

## Config Example

```toml
device = "/dev/input/event3"
master_volume = 0.8
max_voices = 32
samples_dir = "/home/user/.config/keyboard-drums/samples"

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

[[bindings]]
key = "KEY_F"
sample = "hihat_open.wav"
gain = 0.7

[[bindings]]
key = "KEY_J"
sample = "tom_high.wav"
gain = 0.85

[[bindings]]
key = "KEY_K"
sample = "tom_low.wav"
gain = 0.85

[[bindings]]
key = "KEY_L"
sample = "crash.wav"
gain = 0.6

[[bindings]]
key = "KEY_SEMICOLON"
sample = "ride.wav"
gain = 0.65
```
