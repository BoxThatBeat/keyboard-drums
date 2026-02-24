use anyhow::{Context, Result, bail};
use evdev::KeyCode;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Top-level configuration loaded from TOML.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// Path to the evdev keyboard device (e.g. "/dev/input/event3").
    /// Can be overridden by the --device CLI argument.
    pub device: Option<String>,

    /// Master volume multiplier (0.0 to 1.0). Default: 0.8.
    #[serde(default = "default_master_volume")]
    pub master_volume: f32,

    /// Maximum number of simultaneous voices. Default: 32.
    #[serde(default = "default_max_voices")]
    pub max_voices: usize,

    /// Directory containing .wav sample files.
    pub samples_dir: String,

    /// Keybindings mapping evdev key names to sample files.
    pub bindings: Vec<BindingConfig>,
}

/// A single keybinding entry from config.
#[derive(Debug, Deserialize)]
pub struct BindingConfig {
    /// evdev key name (e.g. "KEY_A", "KEY_SPACE").
    pub key: String,

    /// WAV filename relative to samples_dir.
    pub sample: String,

    /// Per-sample gain multiplier (0.0 to 1.0). Default: 1.0.
    #[serde(default = "default_gain")]
    pub gain: f32,
}

/// A validated and resolved keybinding ready for use.
#[derive(Debug, Clone)]
pub struct ResolvedBinding {
    /// evdev key code for this binding.
    pub key_code: KeyCode,

    /// Index into the loaded samples array.
    pub sample_index: usize,

    /// Per-sample gain (already clamped to 0.0..=1.0).
    pub gain: f32,
}

/// Validated configuration with resolved key codes and sample paths.
#[derive(Debug)]
pub struct ResolvedConfig {
    /// Device path (may be None if to be provided by CLI).
    pub device: Option<String>,

    /// Master volume (clamped to 0.0..=1.0).
    pub master_volume: f32,

    /// Maximum simultaneous voices.
    pub max_voices: usize,

    /// Directory containing samples.
    pub samples_dir: PathBuf,

    /// Unique sample file paths in load order (index = sample_index).
    pub sample_files: Vec<PathBuf>,

    /// Map from evdev key code to resolved binding.
    pub key_map: HashMap<u16, ResolvedBinding>,
}

fn default_master_volume() -> f32 {
    0.8
}

fn default_max_voices() -> usize {
    32
}

fn default_gain() -> f32 {
    1.0
}

/// Load and validate configuration from a TOML file.
pub fn load_config(path: &Path) -> Result<ResolvedConfig> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("Failed to read config file: {}", path.display()))?;

    let config: Config =
        toml::from_str(&content).with_context(|| format!("Failed to parse config file: {}", path.display()))?;

    resolve_config(config)
}

/// Validate raw config and resolve key names to key codes and sample paths.
fn resolve_config(config: Config) -> Result<ResolvedConfig> {
    let master_volume = config.master_volume.clamp(0.0, 1.0);
    if (master_volume - config.master_volume).abs() > f32::EPSILON {
        log::warn!(
            "master_volume {} clamped to {}",
            config.master_volume,
            master_volume
        );
    }

    let max_voices = if config.max_voices == 0 {
        log::warn!("max_voices was 0, defaulting to 32");
        32
    } else {
        config.max_voices
    };

    let samples_dir = PathBuf::from(&config.samples_dir);
    if !samples_dir.is_dir() {
        bail!(
            "samples_dir does not exist or is not a directory: {}",
            samples_dir.display()
        );
    }

    // Deduplicate sample files and build index map.
    // Multiple bindings can reference the same sample file — we only load it once.
    let mut sample_files: Vec<PathBuf> = Vec::new();
    let mut sample_name_to_index: HashMap<String, usize> = HashMap::new();
    let mut key_map: HashMap<u16, ResolvedBinding> = HashMap::new();

    if config.bindings.is_empty() {
        bail!("No keybindings defined in config");
    }

    for binding in &config.bindings {
        // Resolve evdev key name to key code.
        let key_code = KeyCode::from_str(&binding.key)
            .map_err(|_| anyhow::anyhow!("Unknown evdev key name: '{}'. Use names like KEY_A, KEY_SPACE, etc.", binding.key))?;

        // Resolve sample file path.
        let sample_path = samples_dir.join(&binding.sample);
        if !sample_path.is_file() {
            bail!(
                "Sample file not found: {} (resolved to {})",
                binding.sample,
                sample_path.display()
            );
        }

        // Get or create sample index.
        let sample_index = if let Some(&idx) = sample_name_to_index.get(&binding.sample) {
            idx
        } else {
            let idx = sample_files.len();
            sample_files.push(sample_path);
            sample_name_to_index.insert(binding.sample.clone(), idx);
            idx
        };

        let gain = binding.gain.clamp(0.0, 1.0);
        if (gain - binding.gain).abs() > f32::EPSILON {
            log::warn!(
                "gain for key {} clamped from {} to {}",
                binding.key,
                binding.gain,
                gain
            );
        }

        let code = key_code.code();
        if key_map.contains_key(&code) {
            log::warn!(
                "Duplicate keybinding for {}: overwriting previous binding",
                binding.key
            );
        }

        key_map.insert(
            code,
            ResolvedBinding {
                key_code,
                sample_index,
                gain,
            },
        );
    }

    log::info!(
        "Config loaded: {} bindings, {} unique samples, master_volume={}, max_voices={}",
        key_map.len(),
        sample_files.len(),
        master_volume,
        max_voices,
    );

    Ok(ResolvedConfig {
        device: config.device,
        master_volume,
        max_voices,
        samples_dir,
        sample_files,
        key_map,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Helper to create a minimal valid config in a temp directory.
    fn setup_test_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let samples_dir = dir.path().join("samples");
        fs::create_dir(&samples_dir).unwrap();

        // Create a minimal valid WAV file (44 bytes header + 0 data).
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let path = samples_dir.join("kick.wav");
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        // Write a single sample so it's a valid WAV.
        writer.write_sample(0i16).unwrap();
        writer.finalize().unwrap();

        dir
    }

    #[test]
    fn test_parse_minimal_config() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = setup_test_dir();
        let samples_dir = dir.path().join("samples");

        let config_str = format!(
            r#"
            samples_dir = "{}"
            master_volume = 0.8
            max_voices = 16

            [[bindings]]
            key = "KEY_A"
            sample = "kick.wav"
            gain = 1.0
            "#,
            samples_dir.display()
        );

        let config: Config = toml::from_str(&config_str).unwrap();
        let resolved = resolve_config(config).unwrap();

        assert_eq!(resolved.master_volume, 0.8);
        assert_eq!(resolved.max_voices, 16);
        assert_eq!(resolved.sample_files.len(), 1);
        assert_eq!(resolved.key_map.len(), 1);
        assert!(resolved.key_map.contains_key(&KeyCode::KEY_A.code()));
    }

    #[test]
    fn test_gain_defaults_to_one() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = setup_test_dir();
        let samples_dir = dir.path().join("samples");

        let config_str = format!(
            r#"
            samples_dir = "{}"

            [[bindings]]
            key = "KEY_A"
            sample = "kick.wav"
            "#,
            samples_dir.display()
        );

        let config: Config = toml::from_str(&config_str).unwrap();
        let resolved = resolve_config(config).unwrap();

        let binding = resolved.key_map.get(&KeyCode::KEY_A.code()).unwrap();
        assert_eq!(binding.gain, 1.0);
    }

    #[test]
    fn test_gain_clamped() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = setup_test_dir();
        let samples_dir = dir.path().join("samples");

        let config_str = format!(
            r#"
            samples_dir = "{}"

            [[bindings]]
            key = "KEY_A"
            sample = "kick.wav"
            gain = 5.0
            "#,
            samples_dir.display()
        );

        let config: Config = toml::from_str(&config_str).unwrap();
        let resolved = resolve_config(config).unwrap();

        let binding = resolved.key_map.get(&KeyCode::KEY_A.code()).unwrap();
        assert_eq!(binding.gain, 1.0);
    }

    #[test]
    fn test_invalid_key_name() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = setup_test_dir();
        let samples_dir = dir.path().join("samples");

        let config_str = format!(
            r#"
            samples_dir = "{}"

            [[bindings]]
            key = "KEY_FOOBAR"
            sample = "kick.wav"
            "#,
            samples_dir.display()
        );

        let config: Config = toml::from_str(&config_str).unwrap();
        let result = resolve_config(config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unknown evdev key name"));
    }

    #[test]
    fn test_missing_sample_file() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = setup_test_dir();
        let samples_dir = dir.path().join("samples");

        let config_str = format!(
            r#"
            samples_dir = "{}"

            [[bindings]]
            key = "KEY_A"
            sample = "nonexistent.wav"
            "#,
            samples_dir.display()
        );

        let config: Config = toml::from_str(&config_str).unwrap();
        let result = resolve_config(config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Sample file not found"));
    }

    #[test]
    fn test_no_bindings() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = setup_test_dir();
        let samples_dir = dir.path().join("samples");

        let config_str = format!(
            r#"
            samples_dir = "{}"
            bindings = []
            "#,
            samples_dir.display()
        );

        let config: Config = toml::from_str(&config_str).unwrap();
        let result = resolve_config(config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No keybindings"));
    }

    #[test]
    fn test_duplicate_sample_deduplication() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = setup_test_dir();
        let samples_dir = dir.path().join("samples");

        let config_str = format!(
            r#"
            samples_dir = "{}"

            [[bindings]]
            key = "KEY_A"
            sample = "kick.wav"

            [[bindings]]
            key = "KEY_S"
            sample = "kick.wav"
            "#,
            samples_dir.display()
        );

        let config: Config = toml::from_str(&config_str).unwrap();
        let resolved = resolve_config(config).unwrap();

        // Two bindings but only one unique sample loaded.
        assert_eq!(resolved.key_map.len(), 2);
        assert_eq!(resolved.sample_files.len(), 1);

        let binding_a = resolved.key_map.get(&KeyCode::KEY_A.code()).unwrap();
        let binding_s = resolved.key_map.get(&KeyCode::KEY_S.code()).unwrap();
        assert_eq!(binding_a.sample_index, binding_s.sample_index);
    }
}
