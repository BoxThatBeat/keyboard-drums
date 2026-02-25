mod audio;
mod config;
mod input;
mod ring;
mod samples;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use clap::Parser;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Ultra-low latency console drum sampler using keyboard input via evdev.
#[derive(Parser, Debug)]
#[command(name = "keyboard-drums", version, about)]
struct Cli {
    /// Path to config file.
    #[arg(short, long, default_value = "~/.config/keyboard-drums/config.toml")]
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
    let config_path = config::expand_tilde(&cli.config);
    log::info!("Loading config from: {}", config_path.display());

    let mut resolved = config::load_config(&config_path)?;

    // CLI --device overrides config.
    if let Some(ref device) = cli.device {
        resolved.device = Some(device.clone());
    }

    // Determine device path: CLI flag > config file > interactive picker.
    let device_path = if let Some(ref path) = resolved.device {
        path.clone()
    } else {
        input::pick_device_interactive()?
    };

    // Build per-sample gain array from config bindings.
    let mut sample_gains = vec![1.0f32; resolved.sample_names.len()];
    for binding in resolved.key_map.values() {
        if binding.sample_index < sample_gains.len() {
            sample_gains[binding.sample_index] = binding.gain;
        }
    }

    // Discover drum kits and variants in the samples directory.
    let library =
        samples::discover_kits(&resolved.samples_dir, &resolved.sample_names, &sample_gains)?;

    // Load the initial sample bank (first kit, first variant).
    let initial_bank = library.load_bank(0, 0)?;
    log::info!(
        "Initial kit: '{}' variant '{}'",
        initial_bank.kit_name,
        initial_bank.variant_name,
    );

    // Create the shared, atomically-swappable sample bank.
    let sample_bank = Arc::new(ArcSwap::from_pointee(initial_bank));

    // Create trigger ring buffer.
    let (producer, consumer) = ring::create_trigger_channel();

    // Build key map for the input thread.
    let key_map = input::build_key_map(&resolved.key_map);

    // Build the set of keys to suppress (sample bindings + cycling keys).
    let suppressed_keys = input::build_suppressed_keys(&key_map, &resolved.cycling_keys);
    log::info!(
        "Suppressing {} bound keys from reaching other applications",
        suppressed_keys.len(),
    );

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
        sample_bank: Arc::clone(&sample_bank),
        max_voices: resolved.max_voices,
        master_volume: resolved.master_volume,
    };

    let _audio_stream = audio::start_audio_stream(audio_config, consumer)?;

    // Open the input device.
    let device = input::open_device(std::path::Path::new(&device_path))?;

    // Create a virtual device mirroring the physical keyboard's capabilities
    // to forward non-bound events (keys, mouse axes, etc.).
    let virtual_device = input::create_virtual_device(&device)?;

    // Run input loop on a dedicated thread using crossbeam scoped threads.
    // This ensures the thread is joined before we exit.
    log::info!("keyboard-drums ready. Press bound keys to play samples.");

    crossbeam::thread::scope(|s| {
        let shutdown_ref = &shutdown;
        let cycling_keys = &resolved.cycling_keys;

        let input_handle = s.spawn(move |_| {
            input::run_input_loop(
                device,
                &key_map,
                producer,
                shutdown_ref,
                cycling_keys,
                library,
                sample_bank,
                &suppressed_keys,
                virtual_device,
            )
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
