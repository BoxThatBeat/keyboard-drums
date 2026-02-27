use crate::ring::{Trigger, TriggerConsumer};
use crate::samples::{SampleBank, SampleData};
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, StreamConfig};
use std::sync::Arc;

/// The output sample rate in Hz.
const OUTPUT_SAMPLE_RATE: u32 = 48_000;

/// The number of output channels (stereo).
const OUTPUT_CHANNELS: u16 = 2;

/// Minimum buffer size in frames. The device-reported minimum can be as low
/// as 1, which causes catastrophic CPU overhead (48k callbacks/sec). 64 frames
/// at 48kHz ≈ 1.3ms — well within the latency budget and realistic for ALSA.
const MIN_BUFFER_FRAMES: u32 = 64;

/// A single active voice (playing sample instance).
#[derive(Debug)]
struct Voice {
    /// Index into the samples array.
    sample_id: u8,

    /// Current playback position in frames.
    position: usize,

    /// Combined gain (per-sample gain * master volume * velocity).
    gain: f32,

    /// Snapshot of the sample data for this voice.
    /// Holds an Arc to the SampleBank that was active when this voice started,
    /// so the sample data stays alive even if the bank is swapped mid-playback.
    sample_data: Arc<SampleData>,
}

/// Configuration for the audio engine.
pub struct AudioEngineConfig {
    /// Shared sample bank that can be swapped at runtime.
    pub sample_bank: Arc<ArcSwap<SampleBank>>,

    /// Maximum number of simultaneous voices.
    pub max_voices: usize,

    /// Master volume (0.0 to 1.0).
    pub master_volume: f32,
}

/// Start the audio output stream and return a handle to it.
///
/// The stream will consume triggers from the ring buffer consumer
/// and mix active voices into the audio output.
///
/// The `sample_bank` is read atomically each time a trigger is received,
/// allowing runtime sample swapping without locks in the audio callback.
///
/// Returns the cpal Stream handle. The stream plays until the handle is dropped.
pub fn start_audio_stream(
    config: AudioEngineConfig,
    mut consumer: TriggerConsumer,
) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("No audio output device found")?;

    let device_name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    log::info!("Using audio output device: {}", device_name);

    // Find the best output config: 48kHz, stereo, smallest buffer.
    let stream_config = find_best_config(&device)?;

    log::info!(
        "Audio stream config: {}Hz, {} channels, buffer: {:?}",
        stream_config.sample_rate,
        stream_config.channels,
        stream_config.buffer_size,
    );

    let sample_bank = config.sample_bank;
    let max_voices = config.max_voices;
    let master_volume = config.master_volume;
    let output_channels = stream_config.channels as usize;

    // Pre-allocate voice array and trigger drain buffer outside the callback.
    // These are moved into the closure and reused every callback — no allocations.
    let mut voices: Vec<Voice> = Vec::with_capacity(max_voices);
    let mut trigger_buf: Vec<Trigger> = Vec::with_capacity(128);

    let stream = device
        .build_output_stream(
            &stream_config,
            move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                audio_callback(
                    data,
                    output_channels,
                    &mut consumer,
                    &mut trigger_buf,
                    &mut voices,
                    &sample_bank,
                    master_volume,
                    max_voices,
                );
            },
            move |err| {
                log::error!("Audio stream error: {}", err);
            },
            None, // No timeout
        )
        .context("Failed to build audio output stream")?;

    stream.play().context("Failed to start audio playback")?;
    log::info!("Audio stream started");

    Ok(stream)
}

/// Find the best output config targeting 48kHz stereo with the smallest buffer.
fn find_best_config(device: &cpal::Device) -> Result<StreamConfig> {
    let supported = device
        .supported_output_configs()
        .context("Failed to query supported output configs")?;

    // Look for a config that supports our target sample rate and is stereo (or at least exists).
    let mut best: Option<cpal::SupportedStreamConfigRange> = None;

    for config in supported {
        // Must support our sample rate.
        if config.min_sample_rate() > OUTPUT_SAMPLE_RATE
            || config.max_sample_rate() < OUTPUT_SAMPLE_RATE
        {
            continue;
        }

        // Must support f32 sample format.
        if config.sample_format() != cpal::SampleFormat::F32 {
            continue;
        }

        // Prefer stereo.
        match &best {
            None => best = Some(config),
            Some(current) => {
                let current_stereo = current.channels() == OUTPUT_CHANNELS;
                let new_stereo = config.channels() == OUTPUT_CHANNELS;
                if new_stereo && !current_stereo {
                    best = Some(config);
                }
            }
        }
    }

    let supported_config = best.context(
        "No supported audio output config found for 48kHz f32. \
         Check that your audio device supports 48kHz output.",
    )?;

    // Request a small buffer size for low latency, but enforce a sane floor.
    // Device-reported minimums can be as low as 1 frame, which causes the
    // callback to fire tens of thousands of times per second — overwhelming
    // the CPU with scheduling overhead and producing no usable audio.
    let buffer_size = match supported_config.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            let target = (*min).max(MIN_BUFFER_FRAMES).min(*max);
            log::info!(
                "Audio device buffer range: {}-{} frames, requesting {} frames ({:.1}ms)",
                min,
                max,
                target,
                target as f64 / OUTPUT_SAMPLE_RATE as f64 * 1000.0,
            );
            BufferSize::Fixed(target)
        }
        cpal::SupportedBufferSize::Unknown => {
            log::info!("Audio device buffer size unknown, using default");
            BufferSize::Default
        }
    };

    let config = StreamConfig {
        channels: supported_config.channels(),
        sample_rate: OUTPUT_SAMPLE_RATE,
        buffer_size,
    };

    Ok(config)
}

/// The core audio callback. Called by cpal on the audio thread.
///
/// This function MUST be real-time safe:
/// - No heap allocations
/// - No locks/mutexes
/// - No syscalls
/// - No logging (except in rare error paths)
///
/// The `ArcSwap::load` is lock-free — it performs an atomic pointer read
/// and increments a reference count. This is safe for real-time audio.
#[inline]
fn audio_callback(
    data: &mut [f32],
    output_channels: usize,
    consumer: &mut TriggerConsumer,
    trigger_buf: &mut Vec<Trigger>,
    voices: &mut Vec<Voice>,
    sample_bank: &Arc<ArcSwap<SampleBank>>,
    master_volume: f32,
    max_voices: usize,
) {
    // 1. Drain all pending triggers from the ring buffer.
    consumer.drain(trigger_buf);

    // 2. Spawn new voices for each trigger.
    if !trigger_buf.is_empty() {
        // Load the current sample bank once per callback (atomic pointer read).
        let bank = sample_bank.load();

        // Voice stealing: free up slots for incoming triggers by removing
        // the oldest voices in a single drain() call. The old code used
        // `while voices.len() > max { voices.remove(0) }` which was O(k*n)
        // (each remove shifts all elements) and caused audio callback
        // overruns under sustained playing.
        let available = max_voices.saturating_sub(voices.len());
        if trigger_buf.len() > available && !voices.is_empty() {
            let to_steal = (trigger_buf.len() - available).min(voices.len());
            voices.drain(..to_steal);
        }

        // Track how many voices we can still add without exceeding max_voices.
        let mut slots_remaining = max_voices.saturating_sub(voices.len());

        for trigger in trigger_buf.iter() {
            if slots_remaining == 0 {
                break;
            }

            let sid = trigger.sample_id as usize;
            if sid >= bank.samples.len() {
                continue; // Invalid sample_id, skip.
            }

            let per_sample_gain = bank.sample_gains.get(sid).copied().unwrap_or(1.0);
            let gain = per_sample_gain * trigger.velocity * master_volume;

            voices.push(Voice {
                sample_id: trigger.sample_id,
                position: 0,
                gain,
                // Clone the Arc to the sample data so this voice keeps
                // a reference even if the bank is swapped while playing.
                sample_data: Arc::clone(&bank.samples[sid]),
            });

            slots_remaining -= 1;
        }
    }

    // 3. Zero the output buffer.
    for sample in data.iter_mut() {
        *sample = 0.0;
    }

    // 4. Mix all active voices into the output buffer.
    let num_frames = data.len() / output_channels;

    let mut i = 0;
    while i < voices.len() {
        let voice = &mut voices[i];
        let sample = &voice.sample_data;
        let sample_channels = sample.channels as usize;
        let sample_frames = sample.num_frames();

        if voice.position >= sample_frames {
            voices.swap_remove(i);
            continue;
        }

        let gain = voice.gain;
        let frames_to_mix = num_frames.min(sample_frames - voice.position);

        // Mix sample data into the output buffer.
        for frame in 0..frames_to_mix {
            let src_frame = voice.position + frame;
            let src_offset = src_frame * sample_channels;

            for ch in 0..output_channels {
                let dst_idx = frame * output_channels + ch;

                // Map output channel to source channel.
                // Mono: duplicate to both channels.
                // Stereo: direct mapping.
                let src_ch = if sample_channels == 1 {
                    0
                } else {
                    ch.min(sample_channels - 1)
                };
                let src_idx = src_offset + src_ch;

                if src_idx < sample.data.len() && dst_idx < data.len() {
                    data[dst_idx] += sample.data[src_idx] * gain;
                }
            }
        }

        voice.position += frames_to_mix;

        // If the voice has finished, remove it.
        if voice.position >= sample_frames {
            voices.swap_remove(i);
        } else {
            i += 1;
        }
    }

    // 5. Clamp output to [-1.0, 1.0] to prevent clipping.
    for sample in data.iter_mut() {
        *sample = sample.clamp(-1.0, 1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring;

    /// Create a simple test sample: a mono sine-like ramp.
    fn make_test_sample(num_frames: usize, channels: u16) -> Arc<SampleData> {
        let total_samples = num_frames * channels as usize;
        let mut data = Vec::with_capacity(total_samples);
        for i in 0..total_samples {
            data.push((i as f32 / total_samples as f32) * 0.5);
        }
        Arc::new(SampleData {
            data,
            channels,
            sample_rate: 48000,
        })
    }

    /// Create a SampleBank wrapped in ArcSwap for testing.
    fn make_test_bank(samples: Vec<Arc<SampleData>>) -> Arc<ArcSwap<SampleBank>> {
        let gains = vec![1.0f32; samples.len()];
        Arc::new(ArcSwap::from_pointee(SampleBank {
            samples,
            sample_gains: gains,
            kit_name: "test".to_string(),
            variant_name: "v1".to_string(),
        }))
    }

    fn make_test_bank_with_gains(
        samples: Vec<Arc<SampleData>>,
        gains: Vec<f32>,
    ) -> Arc<ArcSwap<SampleBank>> {
        Arc::new(ArcSwap::from_pointee(SampleBank {
            samples,
            sample_gains: gains,
            kit_name: "test".to_string(),
            variant_name: "v1".to_string(),
        }))
    }

    #[test]
    fn test_audio_callback_silence_when_no_triggers() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (_prod, mut cons) = ring::create_trigger_channel();
        let bank = make_test_bank(vec![make_test_sample(100, 1)]);
        let mut voices = Vec::with_capacity(32);
        let mut trigger_buf = Vec::with_capacity(128);
        let mut output = vec![0.5f32; 256]; // Pre-fill with non-zero to verify it's zeroed.

        audio_callback(
            &mut output,
            2,
            &mut cons,
            &mut trigger_buf,
            &mut voices,
            &bank,
            1.0,
            32,
        );

        // Output should be all zeros (no voices playing).
        for &s in &output {
            assert_eq!(s, 0.0);
        }
    }

    #[test]
    fn test_audio_callback_plays_triggered_sample() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = ring::create_trigger_channel();
        let bank = make_test_bank(vec![make_test_sample(100, 1)]);
        let mut voices = Vec::with_capacity(32);
        let mut trigger_buf = Vec::with_capacity(128);
        let mut output = vec![0.0f32; 20]; // 10 frames stereo

        // Send a trigger.
        prod.send(Trigger {
            sample_id: 0,
            velocity: 1.0,
        });

        audio_callback(
            &mut output,
            2,
            &mut cons,
            &mut trigger_buf,
            &mut voices,
            &bank,
            1.0,
            32,
        );

        // Output should have non-zero values (sample was mixed in).
        let has_nonzero = output.iter().any(|&s| s != 0.0);
        assert!(has_nonzero, "Expected non-zero output after trigger");
    }

    #[test]
    fn test_voice_finishes_and_is_removed() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = ring::create_trigger_channel();
        let bank = make_test_bank(vec![make_test_sample(5, 1)]);
        let mut voices = Vec::with_capacity(32);
        let mut trigger_buf = Vec::with_capacity(128);

        prod.send(Trigger {
            sample_id: 0,
            velocity: 1.0,
        });

        // First callback: 10 frames output, but sample is only 5 frames.
        let mut output = vec![0.0f32; 20]; // 10 stereo frames
        audio_callback(
            &mut output,
            2,
            &mut cons,
            &mut trigger_buf,
            &mut voices,
            &bank,
            1.0,
            32,
        );

        // Voice should be removed after finishing.
        assert_eq!(voices.len(), 0, "Voice should be removed after sample ends");
    }

    #[test]
    fn test_voice_stealing() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = ring::create_trigger_channel();
        let bank = make_test_bank(vec![make_test_sample(1000, 1)]);
        let mut voices = Vec::with_capacity(4);
        let mut trigger_buf = Vec::with_capacity(128);
        let max_voices = 2;

        // Send 4 triggers but max_voices is 2.
        for _ in 0..4 {
            prod.send(Trigger {
                sample_id: 0,
                velocity: 1.0,
            });
        }

        let mut output = vec![0.0f32; 20];
        audio_callback(
            &mut output,
            2,
            &mut cons,
            &mut trigger_buf,
            &mut voices,
            &bank,
            1.0,
            max_voices,
        );

        assert!(
            voices.len() <= max_voices,
            "Voice count {} exceeds max {}",
            voices.len(),
            max_voices
        );
    }

    #[test]
    fn test_master_volume() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod_full, mut cons_full) = ring::create_trigger_channel();
        let (mut prod_half, mut cons_half) = ring::create_trigger_channel();

        // Use a constant sample (all 0.5).
        let sample = Arc::new(SampleData {
            data: vec![0.5; 100],
            channels: 1,
            sample_rate: 48000,
        });
        let bank = make_test_bank(vec![sample]);

        // Full volume.
        prod_full.send(Trigger {
            sample_id: 0,
            velocity: 1.0,
        });
        let mut output_full = vec![0.0f32; 20];
        let mut voices_full = Vec::with_capacity(32);
        let mut trigger_buf_full = Vec::with_capacity(128);
        audio_callback(
            &mut output_full,
            2,
            &mut cons_full,
            &mut trigger_buf_full,
            &mut voices_full,
            &bank,
            1.0,
            32,
        );

        // Half volume.
        prod_half.send(Trigger {
            sample_id: 0,
            velocity: 1.0,
        });
        let mut output_half = vec![0.0f32; 20];
        let mut voices_half = Vec::with_capacity(32);
        let mut trigger_buf_half = Vec::with_capacity(128);
        audio_callback(
            &mut output_half,
            2,
            &mut cons_half,
            &mut trigger_buf_half,
            &mut voices_half,
            &bank,
            0.5,
            32,
        );

        // Half-volume output should be half of full-volume output.
        for i in 0..output_full.len() {
            if output_full[i] != 0.0 {
                let ratio = output_half[i] / output_full[i];
                assert!(
                    (ratio - 0.5).abs() < 0.01,
                    "Expected half volume ratio, got {} at index {}",
                    ratio,
                    i
                );
            }
        }
    }

    #[test]
    fn test_output_clamping() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = ring::create_trigger_channel();

        // Create a sample with values > 1.0 when summed.
        let loud_sample = Arc::new(SampleData {
            data: vec![0.9; 100],
            channels: 1,
            sample_rate: 48000,
        });
        let bank = make_test_bank(vec![loud_sample]);

        // Send 3 triggers — they'll stack and sum to ~2.7.
        for _ in 0..3 {
            prod.send(Trigger {
                sample_id: 0,
                velocity: 1.0,
            });
        }

        let mut output = vec![0.0f32; 20];
        let mut voices = Vec::with_capacity(32);
        let mut trigger_buf = Vec::with_capacity(128);
        audio_callback(
            &mut output,
            2,
            &mut cons,
            &mut trigger_buf,
            &mut voices,
            &bank,
            1.0,
            32,
        );

        // All values should be clamped to [-1.0, 1.0].
        for &s in &output {
            assert!(
                s >= -1.0 && s <= 1.0,
                "Output sample {} exceeds [-1.0, 1.0]",
                s
            );
        }
    }

    #[test]
    fn test_mono_to_stereo_upmix() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = ring::create_trigger_channel();

        let mono_sample = Arc::new(SampleData {
            data: vec![0.5; 10],
            channels: 1,
            sample_rate: 48000,
        });
        let bank = make_test_bank(vec![mono_sample]);

        prod.send(Trigger {
            sample_id: 0,
            velocity: 1.0,
        });

        // 5 frames stereo = 10 output samples.
        let mut output = vec![0.0f32; 10];
        let mut voices = Vec::with_capacity(32);
        let mut trigger_buf = Vec::with_capacity(128);
        audio_callback(
            &mut output,
            2,
            &mut cons,
            &mut trigger_buf,
            &mut voices,
            &bank,
            1.0,
            32,
        );

        // Mono should be duplicated to both L and R channels.
        for frame in 0..5 {
            let l = output[frame * 2];
            let r = output[frame * 2 + 1];
            assert_eq!(
                l, r,
                "Mono upmix: L and R should be equal at frame {}",
                frame
            );
            assert!(l > 0.0, "Expected non-zero output at frame {}", frame);
        }
    }

    #[test]
    fn test_polyphonic_stacking() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod_single, mut cons_single) = ring::create_trigger_channel();
        let (mut prod_double, mut cons_double) = ring::create_trigger_channel();

        let sample = Arc::new(SampleData {
            data: vec![0.3; 100],
            channels: 1,
            sample_rate: 48000,
        });
        let bank = make_test_bank(vec![sample]);

        // Single trigger.
        prod_single.send(Trigger {
            sample_id: 0,
            velocity: 1.0,
        });
        let mut out_single = vec![0.0f32; 20];
        let mut voices_single = Vec::with_capacity(32);
        let mut tb_single = Vec::with_capacity(128);
        audio_callback(
            &mut out_single,
            2,
            &mut cons_single,
            &mut tb_single,
            &mut voices_single,
            &bank,
            1.0,
            32,
        );

        // Double trigger (two stacked voices).
        prod_double.send(Trigger {
            sample_id: 0,
            velocity: 1.0,
        });
        prod_double.send(Trigger {
            sample_id: 0,
            velocity: 1.0,
        });
        let mut out_double = vec![0.0f32; 20];
        let mut voices_double = Vec::with_capacity(32);
        let mut tb_double = Vec::with_capacity(128);
        audio_callback(
            &mut out_double,
            2,
            &mut cons_double,
            &mut tb_double,
            &mut voices_double,
            &bank,
            1.0,
            32,
        );

        // Double should be approximately 2x single.
        for i in 0..out_single.len() {
            if out_single[i] != 0.0 {
                let ratio = out_double[i] / out_single[i];
                assert!(
                    (ratio - 2.0).abs() < 0.01,
                    "Expected 2x stacking ratio, got {} at index {}",
                    ratio,
                    i
                );
            }
        }
    }

    #[test]
    fn test_bank_swap_mid_playback() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut prod, mut cons) = ring::create_trigger_channel();

        // Start with a long sample.
        let sample_a = Arc::new(SampleData {
            data: vec![0.5; 1000],
            channels: 1,
            sample_rate: 48000,
        });
        let bank = Arc::new(ArcSwap::from_pointee(SampleBank {
            samples: vec![sample_a],
            sample_gains: vec![1.0],
            kit_name: "kit_a".to_string(),
            variant_name: "v1".to_string(),
        }));

        prod.send(Trigger {
            sample_id: 0,
            velocity: 1.0,
        });

        let mut voices = Vec::with_capacity(32);
        let mut trigger_buf = Vec::with_capacity(128);

        // First callback — voice starts playing sample_a.
        let mut output1 = vec![0.0f32; 20];
        audio_callback(
            &mut output1,
            2,
            &mut cons,
            &mut trigger_buf,
            &mut voices,
            &bank,
            1.0,
            32,
        );
        assert_eq!(voices.len(), 1, "Should have 1 playing voice");
        let has_audio = output1.iter().any(|&s| s != 0.0);
        assert!(has_audio, "Should produce audio");

        // Swap to a completely different bank.
        let sample_b = Arc::new(SampleData {
            data: vec![0.9; 500],
            channels: 1,
            sample_rate: 48000,
        });
        bank.store(Arc::new(SampleBank {
            samples: vec![sample_b],
            sample_gains: vec![1.0],
            kit_name: "kit_b".to_string(),
            variant_name: "v1".to_string(),
        }));

        // Second callback — voice should still play sample_a (its Arc snapshot).
        let mut output2 = vec![0.0f32; 20];
        audio_callback(
            &mut output2,
            2,
            &mut cons,
            &mut trigger_buf,
            &mut voices,
            &bank,
            1.0,
            32,
        );
        assert_eq!(voices.len(), 1, "Voice should still be playing");

        // The output should be from sample_a (0.5), not sample_b (0.9).
        // With gain=1.0, the output value should be 0.5.
        assert!(
            (output2[0] - 0.5).abs() < 0.01,
            "Voice should still play old sample after bank swap, got {}",
            output2[0],
        );
    }
}
