use anyhow::{bail, Context, Result};
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

    /// Root directory containing drum kit folders.
    /// Structure: samples_dir/<kit>/<variant>/<sample>.wav
    pub samples_dir: String,

    /// Keybindings mapping evdev key names to sample filenames.
    pub bindings: Vec<BindingConfig>,

    /// Optional keybindings for cycling through kits and variants.
    #[serde(default)]
    pub cycling_keys: CyclingKeysConfig,
}

/// A single keybinding entry from config.
#[derive(Debug, Deserialize)]
pub struct BindingConfig {
    /// evdev key name (e.g. "KEY_A", "KEY_SPACE").
    pub key: String,

    /// WAV filename that must exist in every variant folder (e.g. "kick.wav").
    pub sample: String,

    /// Per-sample gain multiplier (0.0 to 1.0). Default: 1.0.
    #[serde(default = "default_gain")]
    pub gain: f32,
}

/// Keybindings for cycling through drum kits and variants at runtime.
#[derive(Debug, Deserialize, Default)]
pub struct CyclingKeysConfig {
    /// Key to cycle forward through drum kits.
    pub next_kit: Option<String>,

    /// Key to cycle backward through drum kits.
    pub prev_kit: Option<String>,

    /// Key to cycle forward through variants within the current kit.
    pub next_variant: Option<String>,

    /// Key to cycle backward through variants within the current kit.
    pub prev_variant: Option<String>,
}

/// Resolved cycling key codes (validated evdev key codes).
#[derive(Debug, Clone)]
pub struct ResolvedCyclingKeys {
    pub next_kit: Option<u16>,
    pub prev_kit: Option<u16>,
    pub next_variant: Option<u16>,
    pub prev_variant: Option<u16>,
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

    /// Root directory containing drum kit folders.
    pub samples_dir: PathBuf,

    /// Unique sample filenames in load order (index = sample_index).
    /// These are just the filenames (e.g. "kick.wav"), not full paths.
    pub sample_names: Vec<String>,

    /// Map from evdev key code to resolved binding.
    pub key_map: HashMap<u16, ResolvedBinding>,

    /// Resolved cycling keybindings.
    pub cycling_keys: ResolvedCyclingKeys,
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

/// Expand a leading `~` or `~/` to the user's home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" || path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(&path[2..]);
        }
    }
    PathBuf::from(path)
}

/// Resolve an optional evdev key name string to a key code.
fn resolve_optional_key(name: &Option<String>, field: &str) -> Result<Option<u16>> {
    match name {
        None => Ok(None),
        Some(key_name) => {
            let key_code = KeyCode::from_str(key_name).map_err(|_| {
                anyhow::anyhow!(
                    "Unknown evdev key name for {}: '{}'. Use names like KEY_A, KEY_SPACE, etc.",
                    field,
                    key_name,
                )
            })?;
            Ok(Some(key_code.code()))
        }
    }
}

/// Load and validate configuration from a TOML file.
pub fn load_config(path: &Path) -> Result<ResolvedConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;

    let config: Config = toml::from_str(&content)
        .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

    resolve_config(config)
}

/// Validate raw config and resolve key names to key codes.
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

    let samples_dir = expand_tilde(&config.samples_dir);
    if !samples_dir.is_dir() {
        bail!(
            "samples_dir does not exist or is not a directory: {}",
            samples_dir.display()
        );
    }

    // Deduplicate sample names and build index map.
    // Multiple bindings can reference the same sample — we only load it once.
    let mut sample_names: Vec<String> = Vec::new();
    let mut sample_name_to_index: HashMap<String, usize> = HashMap::new();
    let mut key_map: HashMap<u16, ResolvedBinding> = HashMap::new();

    if config.bindings.is_empty() {
        bail!("No keybindings defined in config");
    }

    for binding in &config.bindings {
        // Resolve evdev key name to key code.
        let key_code = KeyCode::from_str(&binding.key).map_err(|_| {
            anyhow::anyhow!(
                "Unknown evdev key name: '{}'. Use names like KEY_A, KEY_SPACE, etc.",
                binding.key
            )
        })?;

        // Get or create sample index by filename.
        let sample_index = if let Some(&idx) = sample_name_to_index.get(&binding.sample) {
            idx
        } else {
            let idx = sample_names.len();
            sample_names.push(binding.sample.clone());
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

    // Resolve cycling keybindings.
    let cycling_keys = ResolvedCyclingKeys {
        next_kit: resolve_optional_key(&config.cycling_keys.next_kit, "next_kit")?,
        prev_kit: resolve_optional_key(&config.cycling_keys.prev_kit, "prev_kit")?,
        next_variant: resolve_optional_key(&config.cycling_keys.next_variant, "next_variant")?,
        prev_variant: resolve_optional_key(&config.cycling_keys.prev_variant, "prev_variant")?,
    };

    // Ensure cycling keys don't collide with sample bindings.
    let cycling_codes: Vec<(u16, &str)> = [
        (cycling_keys.next_kit, "next_kit"),
        (cycling_keys.prev_kit, "prev_kit"),
        (cycling_keys.next_variant, "next_variant"),
        (cycling_keys.prev_variant, "prev_variant"),
    ]
    .iter()
    .filter_map(|(code, name)| code.map(|c| (c, *name)))
    .collect();

    for (code, name) in &cycling_codes {
        if key_map.contains_key(code) {
            bail!(
                "Cycling key '{}' conflicts with a sample keybinding. \
                 Use a different key for cycling.",
                name,
            );
        }
    }

    log::info!(
        "Config loaded: {} bindings, {} unique samples, master_volume={}, max_voices={}",
        key_map.len(),
        sample_names.len(),
        master_volume,
        max_voices,
    );

    Ok(ResolvedConfig {
        device: config
            .device
            .map(|d| expand_tilde(&d).to_string_lossy().into_owned()),
        master_volume,
        max_voices,
        samples_dir,
        sample_names,
        key_map,
        cycling_keys,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Helper to create a minimal valid config test directory with the
    /// new kit/variant folder structure.
    fn setup_test_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let samples_dir = dir.path().join("samples");
        fs::create_dir(&samples_dir).unwrap();

        // Create: samples/acoustic/variant1/kick.wav
        let variant_dir = samples_dir.join("acoustic").join("variant1");
        fs::create_dir_all(&variant_dir).unwrap();

        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let path = variant_dir.join("kick.wav");
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
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
        assert_eq!(resolved.sample_names.len(), 1);
        assert_eq!(resolved.sample_names[0], "kick.wav");
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
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unknown evdev key name"));
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

        // Two bindings but only one unique sample.
        assert_eq!(resolved.key_map.len(), 2);
        assert_eq!(resolved.sample_names.len(), 1);

        let binding_a = resolved.key_map.get(&KeyCode::KEY_A.code()).unwrap();
        let binding_s = resolved.key_map.get(&KeyCode::KEY_S.code()).unwrap();
        assert_eq!(binding_a.sample_index, binding_s.sample_index);
    }

    #[test]
    fn test_cycling_keys_parsed() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = setup_test_dir();
        let samples_dir = dir.path().join("samples");

        let config_str = format!(
            r#"
            samples_dir = "{}"

            [cycling_keys]
            next_kit = "KEY_RIGHT"
            prev_kit = "KEY_LEFT"
            next_variant = "KEY_UP"
            prev_variant = "KEY_DOWN"

            [[bindings]]
            key = "KEY_A"
            sample = "kick.wav"
            "#,
            samples_dir.display()
        );

        let config: Config = toml::from_str(&config_str).unwrap();
        let resolved = resolve_config(config).unwrap();

        assert_eq!(
            resolved.cycling_keys.next_kit,
            Some(KeyCode::KEY_RIGHT.code())
        );
        assert_eq!(
            resolved.cycling_keys.prev_kit,
            Some(KeyCode::KEY_LEFT.code())
        );
        assert_eq!(
            resolved.cycling_keys.next_variant,
            Some(KeyCode::KEY_UP.code())
        );
        assert_eq!(
            resolved.cycling_keys.prev_variant,
            Some(KeyCode::KEY_DOWN.code())
        );
    }

    #[test]
    fn test_cycling_keys_optional() {
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

        assert!(resolved.cycling_keys.next_kit.is_none());
        assert!(resolved.cycling_keys.prev_kit.is_none());
        assert!(resolved.cycling_keys.next_variant.is_none());
        assert!(resolved.cycling_keys.prev_variant.is_none());
    }

    #[test]
    fn test_cycling_key_conflicts_with_binding() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = setup_test_dir();
        let samples_dir = dir.path().join("samples");

        let config_str = format!(
            r#"
            samples_dir = "{}"

            [cycling_keys]
            next_kit = "KEY_A"

            [[bindings]]
            key = "KEY_A"
            sample = "kick.wav"
            "#,
            samples_dir.display()
        );

        let config: Config = toml::from_str(&config_str).unwrap();
        let result = resolve_config(config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("conflicts"));
    }

    #[test]
    fn test_invalid_cycling_key_name() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = setup_test_dir();
        let samples_dir = dir.path().join("samples");

        let config_str = format!(
            r#"
            samples_dir = "{}"

            [cycling_keys]
            next_kit = "KEY_DOESNOTEXIST"

            [[bindings]]
            key = "KEY_A"
            sample = "kick.wav"
            "#,
            samples_dir.display()
        );

        let config: Config = toml::from_str(&config_str).unwrap();
        let result = resolve_config(config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unknown evdev key name"));
    }
}
