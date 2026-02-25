use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// The expected output sample rate. Samples must match this rate.
pub const OUTPUT_SAMPLE_RATE: u32 = 48_000;

/// Preloaded sample data stored in memory for zero-latency playback.
#[derive(Debug)]
pub struct SampleData {
    /// Interleaved f32 samples normalized to [-1.0, 1.0].
    /// For mono: [s0, s1, s2, ...]
    /// For stereo: [L0, R0, L1, R1, ...]
    pub data: Vec<f32>,

    /// Number of channels (1 = mono, 2 = stereo).
    pub channels: u16,

    /// Original sample rate (must equal OUTPUT_SAMPLE_RATE).
    pub sample_rate: u32,
}

impl SampleData {
    /// Number of audio frames (samples per channel).
    pub fn num_frames(&self) -> usize {
        if self.channels == 0 {
            return 0;
        }
        self.data.len() / self.channels as usize
    }

    /// Duration in seconds.
    pub fn duration_secs(&self) -> f64 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        self.num_frames() as f64 / self.sample_rate as f64
    }
}

/// A collection of loaded samples that the audio thread reads atomically.
/// Swapped in as a unit when the user cycles kits or variants.
#[derive(Debug)]
pub struct SampleBank {
    /// Loaded sample data indexed by sample_id.
    pub samples: Vec<Arc<SampleData>>,

    /// Per-sample gain values indexed by sample_id.
    pub sample_gains: Vec<f32>,

    /// Name of the current kit (folder name).
    pub kit_name: String,

    /// Name of the current variant (folder name).
    pub variant_name: String,
}

/// Discovered drum kit with its variants.
#[derive(Debug, Clone)]
pub struct KitInfo {
    /// Kit folder name (e.g. "acoustic").
    pub name: String,

    /// Sorted variant folder names (e.g. ["variant1", "variant2"]).
    pub variants: Vec<String>,
}

/// All discovered kits in the samples directory.
#[derive(Debug, Clone)]
pub struct KitLibrary {
    /// Root samples directory.
    pub samples_dir: PathBuf,

    /// Discovered kits in sorted order.
    pub kits: Vec<KitInfo>,

    /// Sample filenames that bindings expect (e.g. ["kick.wav", "snare.wav"]).
    pub sample_names: Vec<String>,

    /// Per-sample gains from config bindings, indexed by sample_id.
    pub sample_gains: Vec<f32>,
}

impl KitLibrary {
    /// Get the number of kits.
    pub fn kit_count(&self) -> usize {
        self.kits.len()
    }

    /// Get the number of variants for a given kit index.
    pub fn variant_count(&self, kit_index: usize) -> usize {
        self.kits.get(kit_index).map_or(0, |k| k.variants.len())
    }

    /// Build the full path to a variant directory.
    pub fn variant_path(&self, kit_index: usize, variant_index: usize) -> Option<PathBuf> {
        let kit = self.kits.get(kit_index)?;
        let variant = kit.variants.get(variant_index)?;
        Some(self.samples_dir.join(&kit.name).join(variant))
    }

    /// Load all samples for a given kit/variant into a SampleBank.
    ///
    /// Missing sample files are replaced with silent placeholders so that
    /// variants with partial sample coverage still work — the missing
    /// bindings simply produce no sound.
    pub fn load_bank(&self, kit_index: usize, variant_index: usize) -> Result<SampleBank> {
        let kit = self.kits.get(kit_index).context("Kit index out of range")?;
        let variant = kit
            .variants
            .get(variant_index)
            .context("Variant index out of range")?;

        let variant_dir = self.samples_dir.join(&kit.name).join(variant);

        let start = Instant::now();
        let mut samples = Vec::with_capacity(self.sample_names.len());
        let mut loaded_count = 0usize;

        for (i, name) in self.sample_names.iter().enumerate() {
            let path = variant_dir.join(name);

            if path.is_file() {
                log::debug!(
                    "Loading sample {} of {}: {}",
                    i + 1,
                    self.sample_names.len(),
                    path.display()
                );
                let sample = load_wav(&path).with_context(|| {
                    format!(
                        "Failed to load sample '{}' from kit '{}' variant '{}'",
                        name, kit.name, variant,
                    )
                })?;
                samples.push(Arc::new(sample));
                loaded_count += 1;
            } else {
                log::debug!(
                    "Sample '{}' not found in kit '{}' variant '{}' — using silence",
                    name,
                    kit.name,
                    variant,
                );
                samples.push(Arc::new(SampleData {
                    data: vec![],
                    channels: 1,
                    sample_rate: OUTPUT_SAMPLE_RATE,
                }));
            }
        }

        let elapsed = start.elapsed();
        let missing = self.sample_names.len() - loaded_count;
        if missing > 0 {
            log::info!(
                "Loaded {}/{} samples for kit '{}' variant '{}' in {:.1}ms ({} silent)",
                loaded_count,
                self.sample_names.len(),
                kit.name,
                variant,
                elapsed.as_secs_f64() * 1000.0,
                missing,
            );
        } else {
            log::info!(
                "Loaded {} samples for kit '{}' variant '{}' in {:.1}ms",
                loaded_count,
                kit.name,
                variant,
                elapsed.as_secs_f64() * 1000.0,
            );
        }

        Ok(SampleBank {
            samples,
            sample_gains: self.sample_gains.clone(),
            kit_name: kit.name.clone(),
            variant_name: variant.clone(),
        })
    }
}

/// Discover all kits and variants in the samples directory.
///
/// Expected structure:
/// ```text
/// samples_dir/
///   kit_name/
///     variant_name/
///       kick.wav
///       snare.wav
///       ...
/// ```
///
/// Kits and variants are sorted alphabetically. Each variant must contain
/// all of the sample files specified in `sample_names`.
pub fn discover_kits(
    samples_dir: &Path,
    sample_names: &[String],
    sample_gains: &[f32],
) -> Result<KitLibrary> {
    let mut kits: Vec<KitInfo> = Vec::new();

    let entries = std::fs::read_dir(samples_dir).with_context(|| {
        format!(
            "Failed to read samples directory: {}",
            samples_dir.display()
        )
    })?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let kit_name = entry.file_name().to_string_lossy().to_string();

        // Find variants (subdirectories of this kit).
        let mut variants: Vec<String> = Vec::new();
        let variant_entries = std::fs::read_dir(&path)
            .with_context(|| format!("Failed to read kit directory: {}", path.display()))?;

        for ventry in variant_entries {
            let ventry = ventry?;
            let vpath = ventry.path();
            if !vpath.is_dir() {
                continue;
            }

            let variant_name = ventry.file_name().to_string_lossy().to_string();

            // Check which sample files are present in this variant.
            // Variants are accepted even if some samples are missing —
            // missing samples will be silent placeholders at load time.
            let mut present_count = 0;
            for sample_name in sample_names {
                let sample_path = vpath.join(sample_name);
                if sample_path.is_file() {
                    present_count += 1;
                } else {
                    log::info!(
                        "Variant '{}/{}': missing sample '{}' (will be silent)",
                        kit_name,
                        variant_name,
                        sample_name,
                    );
                }
            }

            if present_count > 0 {
                variants.push(variant_name);
            } else {
                log::warn!(
                    "Skipping variant '{}/{}': no sample files found",
                    kit_name,
                    variant_name,
                );
            }
        }

        variants.sort();

        if variants.is_empty() {
            log::warn!("Skipping kit '{}': no valid variants found", kit_name,);
            continue;
        }

        kits.push(KitInfo {
            name: kit_name,
            variants,
        });
    }

    kits.sort_by(|a, b| a.name.cmp(&b.name));

    if kits.is_empty() {
        bail!(
            "No valid drum kits found in {}. Expected structure: \
             samples_dir/<kit>/<variant>/<sample>.wav",
            samples_dir.display(),
        );
    }

    let total_variants: usize = kits.iter().map(|k| k.variants.len()).sum();
    log::info!(
        "Discovered {} kits with {} total variants in {}",
        kits.len(),
        total_variants,
        samples_dir.display(),
    );
    for kit in &kits {
        log::info!(
            "  Kit '{}': {} variants ({:?})",
            kit.name,
            kit.variants.len(),
            kit.variants,
        );
    }

    Ok(KitLibrary {
        samples_dir: samples_dir.to_path_buf(),
        kits,
        sample_names: sample_names.to_vec(),
        sample_gains: sample_gains.to_vec(),
    })
}

/// Load a single WAV file into a SampleData struct.
///
/// The WAV must be 48kHz. Supports 16-bit and 24-bit integer formats,
/// as well as 32-bit float. Mono and stereo are supported.
pub fn load_wav(path: &Path) -> Result<SampleData> {
    let reader = hound::WavReader::open(path)
        .with_context(|| format!("Failed to open WAV file: {}", path.display()))?;

    let spec = reader.spec();

    // Validate sample rate.
    if spec.sample_rate != OUTPUT_SAMPLE_RATE {
        bail!(
            "Sample rate mismatch in {}: expected {}Hz, got {}Hz. \
             Please convert your samples to {}Hz.",
            path.display(),
            OUTPUT_SAMPLE_RATE,
            spec.sample_rate,
            OUTPUT_SAMPLE_RATE,
        );
    }

    // Validate channels.
    if spec.channels == 0 || spec.channels > 2 {
        bail!(
            "Unsupported channel count in {}: {}. Only mono (1) and stereo (2) are supported.",
            path.display(),
            spec.channels,
        );
    }

    let channels = spec.channels;
    let data = decode_samples(reader, &spec, path)?;

    log::info!(
        "Loaded sample: {} ({} channels, {}Hz, {:.2}s, {} frames, {:.1} KB)",
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string()),
        channels,
        spec.sample_rate,
        data.len() as f64 / channels as f64 / spec.sample_rate as f64,
        data.len() / channels as usize,
        data.len() as f64 * 4.0 / 1024.0,
    );

    Ok(SampleData {
        data,
        channels,
        sample_rate: spec.sample_rate,
    })
}

/// Decode WAV samples to normalized f32 based on the sample format and bit depth.
fn decode_samples(
    reader: hound::WavReader<std::io::BufReader<std::fs::File>>,
    spec: &hound::WavSpec,
    path: &Path,
) -> Result<Vec<f32>> {
    match spec.sample_format {
        hound::SampleFormat::Int => {
            let max_val = (1u32 << (spec.bits_per_sample - 1)) as f32;
            let data: Vec<f32> = reader
                .into_samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max_val))
                .collect::<std::result::Result<Vec<f32>, _>>()
                .with_context(|| format!("Failed to decode samples from {}", path.display()))?;
            Ok(data)
        }
        hound::SampleFormat::Float => {
            let data: Vec<f32> = reader
                .into_samples::<f32>()
                .collect::<std::result::Result<Vec<f32>, _>>()
                .with_context(|| {
                    format!("Failed to decode float samples from {}", path.display())
                })?;
            Ok(data)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a test WAV file with known properties.
    fn create_test_wav(
        dir: &Path,
        name: &str,
        channels: u16,
        sample_rate: u32,
        bits_per_sample: u16,
        num_frames: usize,
    ) -> PathBuf {
        let path = dir.join(name);
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();

        for frame in 0..num_frames {
            for _ch in 0..channels {
                // Write a simple ramp signal for testing.
                let value = ((frame as f64 / num_frames as f64) * 32767.0) as i16;
                writer.write_sample(value).unwrap();
            }
        }
        writer.finalize().unwrap();
        path
    }

    fn create_test_wav_f32(
        dir: &Path,
        name: &str,
        channels: u16,
        sample_rate: u32,
        num_frames: usize,
    ) -> PathBuf {
        let path = dir.join(name);
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();

        for frame in 0..num_frames {
            for _ch in 0..channels {
                let value = frame as f32 / num_frames as f32;
                writer.write_sample(value).unwrap();
            }
        }
        writer.finalize().unwrap();
        path
    }

    /// Helper to create a kit/variant directory structure with WAV files.
    fn setup_kit_dir(root: &Path, kit_name: &str, variant_name: &str, sample_names: &[&str]) {
        let variant_dir = root.join(kit_name).join(variant_name);
        std::fs::create_dir_all(&variant_dir).unwrap();
        for &name in sample_names {
            create_test_wav(&variant_dir, name, 1, 48000, 16, 100);
        }
    }

    #[test]
    fn test_load_mono_16bit() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = tempfile::tempdir().unwrap();
        let path = create_test_wav(dir.path(), "mono16.wav", 1, 48000, 16, 1000);

        let sample = load_wav(&path).unwrap();
        assert_eq!(sample.channels, 1);
        assert_eq!(sample.sample_rate, 48000);
        assert_eq!(sample.num_frames(), 1000);
        assert_eq!(sample.data.len(), 1000);
    }

    #[test]
    fn test_load_stereo_16bit() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = tempfile::tempdir().unwrap();
        let path = create_test_wav(dir.path(), "stereo16.wav", 2, 48000, 16, 500);

        let sample = load_wav(&path).unwrap();
        assert_eq!(sample.channels, 2);
        assert_eq!(sample.sample_rate, 48000);
        assert_eq!(sample.num_frames(), 500);
        assert_eq!(sample.data.len(), 1000); // 500 frames * 2 channels
    }

    #[test]
    fn test_load_24bit() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = tempfile::tempdir().unwrap();
        let path = create_test_wav(dir.path(), "mono24.wav", 1, 48000, 24, 100);

        let sample = load_wav(&path).unwrap();
        assert_eq!(sample.channels, 1);
        assert_eq!(sample.num_frames(), 100);
    }

    #[test]
    fn test_load_float32() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = tempfile::tempdir().unwrap();
        let path = create_test_wav_f32(dir.path(), "float32.wav", 1, 48000, 200);

        let sample = load_wav(&path).unwrap();
        assert_eq!(sample.channels, 1);
        assert_eq!(sample.num_frames(), 200);
        // First sample should be 0.0, last should be close to 1.0.
        assert!((sample.data[0] - 0.0).abs() < 0.01);
        assert!((sample.data[199] - 199.0 / 200.0).abs() < 0.01);
    }

    #[test]
    fn test_wrong_sample_rate() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = tempfile::tempdir().unwrap();
        let path = create_test_wav(dir.path(), "wrong_rate.wav", 1, 44100, 16, 100);

        let result = load_wav(&path);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Sample rate mismatch"));
        assert!(err.contains("44100"));
    }

    #[test]
    fn test_normalization_16bit() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = tempfile::tempdir().unwrap();

        // Write a known max-value sample.
        let path = dir.path().join("maxval.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        writer.write_sample(i16::MAX).unwrap();
        writer.write_sample(i16::MIN).unwrap();
        writer.write_sample(0i16).unwrap();
        writer.finalize().unwrap();

        let sample = load_wav(&path).unwrap();
        // i16::MAX / 32768.0 should be very close to 1.0
        assert!((sample.data[0] - (i16::MAX as f32 / 32768.0)).abs() < 0.001);
        // i16::MIN / 32768.0 = -1.0
        assert!((sample.data[1] - (i16::MIN as f32 / 32768.0)).abs() < 0.001);
        // 0 should be 0.0
        assert!((sample.data[2]).abs() < 0.001);
    }

    #[test]
    fn test_duration() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = tempfile::tempdir().unwrap();
        // 48000 frames at 48kHz = exactly 1.0 second.
        let path = create_test_wav(dir.path(), "onesec.wav", 1, 48000, 16, 48000);

        let sample = load_wav(&path).unwrap();
        assert!((sample.duration_secs() - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_nonexistent_file() {
        let _ = env_logger::builder().is_test(true).try_init();
        let result = load_wav(Path::new("/nonexistent/foo.wav"));
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_sample() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = tempfile::tempdir().unwrap();
        let path = create_test_wav(dir.path(), "empty.wav", 1, 48000, 16, 0);

        let sample = load_wav(&path).unwrap();
        assert_eq!(sample.num_frames(), 0);
        assert_eq!(sample.data.len(), 0);
        assert_eq!(sample.duration_secs(), 0.0);
    }

    #[test]
    fn test_discover_kits() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        setup_kit_dir(root, "acoustic", "variant1", &["kick.wav", "snare.wav"]);
        setup_kit_dir(root, "acoustic", "variant2", &["kick.wav", "snare.wav"]);
        setup_kit_dir(root, "electric", "variant1", &["kick.wav", "snare.wav"]);

        let sample_names = vec!["kick.wav".to_string(), "snare.wav".to_string()];
        let sample_gains = vec![1.0, 0.9];
        let library = discover_kits(root, &sample_names, &sample_gains).unwrap();

        assert_eq!(library.kits.len(), 2);
        assert_eq!(library.kits[0].name, "acoustic");
        assert_eq!(library.kits[0].variants, vec!["variant1", "variant2"]);
        assert_eq!(library.kits[1].name, "electric");
        assert_eq!(library.kits[1].variants, vec!["variant1"]);
    }

    #[test]
    fn test_discover_kits_accepts_partial_variants() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // variant1 has both samples, variant2 only has kick.wav
        setup_kit_dir(root, "acoustic", "variant1", &["kick.wav", "snare.wav"]);
        setup_kit_dir(root, "acoustic", "variant2", &["kick.wav"]);

        let sample_names = vec!["kick.wav".to_string(), "snare.wav".to_string()];
        let sample_gains = vec![1.0, 0.9];
        let library = discover_kits(root, &sample_names, &sample_gains).unwrap();

        // Both variants should be accepted — variant2 has partial coverage.
        assert_eq!(library.kits.len(), 1);
        assert_eq!(library.kits[0].variants, vec!["variant1", "variant2"]);
    }

    #[test]
    fn test_discover_kits_skips_empty_variants() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // variant1 has samples, variant2 has no sample files at all.
        setup_kit_dir(root, "acoustic", "variant1", &["kick.wav"]);
        setup_kit_dir(root, "acoustic", "variant2", &[]);

        let sample_names = vec!["kick.wav".to_string(), "snare.wav".to_string()];
        let sample_gains = vec![1.0, 0.9];
        let library = discover_kits(root, &sample_names, &sample_gains).unwrap();

        assert_eq!(library.kits.len(), 1);
        assert_eq!(library.kits[0].variants, vec!["variant1"]);
    }

    #[test]
    fn test_load_bank_with_missing_sample() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Only provide kick.wav, not snare.wav.
        setup_kit_dir(root, "acoustic", "variant1", &["kick.wav"]);

        let sample_names = vec!["kick.wav".to_string(), "snare.wav".to_string()];
        let sample_gains = vec![1.0, 0.8];
        let library = discover_kits(root, &sample_names, &sample_gains).unwrap();

        let bank = library.load_bank(0, 0).unwrap();

        // Both slots should exist in the bank.
        assert_eq!(bank.samples.len(), 2);

        // kick.wav should have real data.
        assert!(bank.samples[0].num_frames() > 0);

        // snare.wav should be a silent placeholder (empty data).
        assert_eq!(bank.samples[1].num_frames(), 0);
        assert_eq!(bank.samples[1].data.len(), 0);
    }

    #[test]
    fn test_discover_kits_no_valid_kits() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = tempfile::tempdir().unwrap();

        let sample_names = vec!["kick.wav".to_string()];
        let sample_gains = vec![1.0];
        let result = discover_kits(dir.path(), &sample_names, &sample_gains);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No valid drum kits"));
    }

    #[test]
    fn test_load_bank() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        setup_kit_dir(root, "acoustic", "variant1", &["kick.wav", "snare.wav"]);

        let sample_names = vec!["kick.wav".to_string(), "snare.wav".to_string()];
        let sample_gains = vec![1.0, 0.8];
        let library = discover_kits(root, &sample_names, &sample_gains).unwrap();

        let bank = library.load_bank(0, 0).unwrap();
        assert_eq!(bank.samples.len(), 2);
        assert_eq!(bank.sample_gains.len(), 2);
        assert_eq!(bank.kit_name, "acoustic");
        assert_eq!(bank.variant_name, "variant1");
        assert!((bank.sample_gains[1] - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn test_kit_library_variant_path() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        setup_kit_dir(root, "acoustic", "variant1", &["kick.wav"]);

        let sample_names = vec!["kick.wav".to_string()];
        let sample_gains = vec![1.0];
        let library = discover_kits(root, &sample_names, &sample_gains).unwrap();

        let path = library.variant_path(0, 0).unwrap();
        assert_eq!(path, root.join("acoustic").join("variant1"));

        assert!(library.variant_path(99, 0).is_none());
        assert!(library.variant_path(0, 99).is_none());
    }
}
