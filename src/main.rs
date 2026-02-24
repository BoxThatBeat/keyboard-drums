mod audio;
mod config;
mod input;
mod ring;
mod samples;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Ultra-low latency console drum sampler using keyboard input via evdev.
#[derive(Parser, Debug)]
#[command(name = "keyboard-drums", version, about)]
struct Cli {
    /// Path to config file.
    #[arg(
        short,
        long,
        default_value = "~/.config/keyboard-drums/config.toml"
    )]
    config: String,

    /// Override the evdev device path from config.
    #[arg(short, long)]
    device: Option<String>,

    /// List available input devices and exit.
    #[arg(long)]
    list_devices: bool,

    /// Enable verbose (debug) logging.
    #[arg(short, long)]
    verbose: bool,
}

fn main() {
    let cli = Cli::parse();

    // Initialize logging.
    let log_level = if cli.verbose { "debug" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level))
        .format_timestamp_millis()
        .init();

    if let Err(e) = run(cli) {
        log::error!("{:#}", e);
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    // Handle --list-devices.
    if cli.list_devices {
        input::list_devices();
        return Ok(());
    }

    // Resolve config path (expand ~ to home dir).
    let config_path = expand_tilde(&cli.config);
    log::info!("Loading config from: {}", config_path.display());

    let mut resolved = config::load_config(&config_path)?;

    // CLI --device overrides config.
    if let Some(ref device) = cli.device {
        resolved.device = Some(device.clone());
    }

    let device_path = resolved
        .device
        .as_ref()
        .context(
            "No device specified. Set 'device' in config.toml or use --device. \
             Use --list-devices to see available devices.",
        )?
        .clone();

    // Preload samples.
    let loaded_samples = samples::preload_samples(&resolved.sample_files)?;

    // Build per-sample gain array for the audio engine.
    // Index by sample_id, gain comes from config bindings.
    let mut sample_gains = vec![1.0f32; loaded_samples.len()];
    for binding in resolved.key_map.values() {
        if binding.sample_index < sample_gains.len() {
            sample_gains[binding.sample_index] = binding.gain;
        }
    }

    // Create trigger ring buffer.
    let (producer, consumer) = ring::create_trigger_channel();

    // Build key map for the input thread.
    let key_map = input::build_key_map(&resolved.key_map);

    // Set up signal handlers.
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_signal = shutdown.clone();

    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown_signal))
        .context("Failed to register SIGTERM handler")?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown_signal))
        .context("Failed to register SIGINT handler")?;

    log::info!("Signal handlers registered (SIGTERM, SIGINT)");

    // Start the audio engine.
    let audio_config = audio::AudioEngineConfig {
        samples: loaded_samples,
        max_voices: resolved.max_voices,
        master_volume: resolved.master_volume,
        sample_gains,
    };

    let _audio_stream = audio::start_audio_stream(audio_config, consumer)?;

    // Open the input device.
    let device = input::open_device(std::path::Path::new(&device_path))?;

    // Run input loop on a dedicated thread using crossbeam scoped threads.
    // This ensures the thread is joined before we exit.
    log::info!("keyboard-drums ready. Press bound keys to play samples.");

    crossbeam::thread::scope(|s| {
        let shutdown_ref = &shutdown;

        let input_handle = s.spawn(move |_| {
            input::run_input_loop(device, &key_map, producer, shutdown_ref)
        });

        // Main thread: wait for shutdown signal.
        while !shutdown.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        log::info!("Shutdown signal received, stopping...");

        // Wait for the input thread to exit.
        match input_handle.join() {
            Ok(Ok(())) => log::info!("Input thread exited cleanly"),
            Ok(Err(e)) => log::error!("Input thread error: {:#}", e),
            Err(_) => log::error!("Input thread panicked"),
        }
    })
    .map_err(|_| anyhow::anyhow!("Thread scope panicked"))?;

    // Audio stream is dropped here, stopping playback.
    log::info!("keyboard-drums stopped.");

    Ok(())
}

/// Expand `~` at the start of a path to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(&path[2..]);
        }
    }
    PathBuf::from(path)
}
