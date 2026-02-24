use anyhow::{Context, Result, bail};
use std::path::Path;
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
                .map(|s| {
                    s.map(|v| v as f32 / max_val)
                })
                .collect::<std::result::Result<Vec<f32>, _>>()
                .with_context(|| format!("Failed to decode samples from {}", path.display()))?;
            Ok(data)
        }
        hound::SampleFormat::Float => {
            let data: Vec<f32> = reader
                .into_samples::<f32>()
                .collect::<std::result::Result<Vec<f32>, _>>()
                .with_context(|| format!("Failed to decode float samples from {}", path.display()))?;
            Ok(data)
        }
    }
}

/// Preload all sample files into memory.
///
/// Returns a Vec of Arc<SampleData> indexed by sample_index (matching the
/// order from ResolvedConfig::sample_files).
pub fn preload_samples(sample_files: &[std::path::PathBuf]) -> Result<Vec<Arc<SampleData>>> {
    let start = Instant::now();
    let mut samples = Vec::with_capacity(sample_files.len());

    for (i, path) in sample_files.iter().enumerate() {
        log::debug!("Loading sample {} of {}: {}", i + 1, sample_files.len(), path.display());
        let sample = load_wav(path)
            .with_context(|| format!("Failed to load sample {}: {}", i, path.display()))?;
        samples.push(Arc::new(sample));
    }

    let elapsed = start.elapsed();
    log::info!(
        "Preloaded {} samples in {:.1}ms",
        samples.len(),
        elapsed.as_secs_f64() * 1000.0,
    );

    Ok(samples)
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
    ) -> std::path::PathBuf {
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
    ) -> std::path::PathBuf {
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
    fn test_preload_multiple() {
        let _ = env_logger::builder().is_test(true).try_init();
        let dir = tempfile::tempdir().unwrap();

        let path1 = create_test_wav(dir.path(), "a.wav", 1, 48000, 16, 100);
        let path2 = create_test_wav(dir.path(), "b.wav", 2, 48000, 16, 200);

        let paths = vec![path1, path2];
        let samples = preload_samples(&paths).unwrap();

        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].channels, 1);
        assert_eq!(samples[0].num_frames(), 100);
        assert_eq!(samples[1].channels, 2);
        assert_eq!(samples[1].num_frames(), 200);
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
}
