#[derive(Debug, thiserror::Error)]
pub enum ResampleError {
    #[error("resampler init failed (ratio={ratio}, chunk={chunk_size}): {reason}")]
    InitFailed {
        ratio: f64,
        chunk_size: usize,
        reason: String,
    },
    #[error("resampler process failed (ratio={ratio}, samples={sample_count}): {reason}")]
    ProcessFailed {
        ratio: f64,
        sample_count: usize,
        reason: String,
    },
}

/// Resample mono audio using sinc interpolation (via rubato).
///
/// Returns `Ok(Cow::Borrowed)` when `from_rate == to_rate` (zero-copy).
/// Uses high-quality `SincFixedIn` resampler with `BlackmanHarris2` window
/// to avoid aliasing when downsampling (e.g. 48kHz → 16kHz for Parakeet).
///
/// This is the single canonical resampler — both `yapstack-transcription-sidecar` and
/// `yapstack-audio::mixer` call through here.
pub fn resample(
    samples: &[f32],
    from_rate: u32,
    to_rate: u32,
) -> Result<std::borrow::Cow<'_, [f32]>, ResampleError> {
    use rubato::{
        Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
    };

    if from_rate == 0 || to_rate == 0 || samples.is_empty() || from_rate == to_rate {
        return Ok(std::borrow::Cow::Borrowed(samples));
    }

    let params = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.98,
        interpolation: SincInterpolationType::Cubic,
        oversampling_factor: 128,
        window: WindowFunction::BlackmanHarris2,
    };

    let ratio = to_rate as f64 / from_rate as f64;
    // chunk_size: process in blocks; use full input length for single-shot
    let chunk_size = samples.len();

    let mut resampler =
        SincFixedIn::<f32>::new(ratio, 2.0, params, chunk_size, 1).map_err(|e| {
            ResampleError::InitFailed {
                ratio,
                chunk_size,
                reason: e.to_string(),
            }
        })?;

    let input = vec![samples.to_vec()];
    let output = resampler
        .process(&input, None)
        .map_err(|e| ResampleError::ProcessFailed {
            ratio,
            sample_count: samples.len(),
            reason: e.to_string(),
        })?;

    if output.is_empty() || output[0].is_empty() {
        return Err(ResampleError::ProcessFailed {
            ratio,
            sample_count: samples.len(),
            reason: "resampler produced empty output".to_string(),
        });
    }

    Ok(std::borrow::Cow::Owned(output.into_iter().next().unwrap()))
}

/// Converts interleaved multi-channel audio to mono by averaging channels per frame.
/// Mono input (channels == 1) borrows the original slice (zero-copy).
/// Multi-channel input allocates a new `Vec<f32>`.
pub fn deinterleave_to_mono(samples: &[f32], channels: u16) -> std::borrow::Cow<'_, [f32]> {
    if channels <= 1 {
        return std::borrow::Cow::Borrowed(samples);
    }
    let ch = channels as usize;
    let usable = samples.len() - samples.len() % ch;
    std::borrow::Cow::Owned(
        samples[..usable]
            .chunks_exact(ch)
            .map(|frame| frame.iter().sum::<f32>() / ch as f32)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    // --- resample tests ---

    #[test]
    fn test_resample_identity() {
        let samples = vec![0.1, 0.2, 0.3, 0.4];
        let result = resample(&samples, 48000, 48000).unwrap();
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(&*result, &samples[..]);
    }

    #[test]
    fn test_resample_empty() {
        let result = resample(&[], 96000, 48000).unwrap();
        assert!(matches!(result, Cow::Borrowed(_)));
        assert!(result.is_empty());
    }

    #[test]
    fn test_resample_zero_rates() {
        let samples = vec![0.1, 0.2, 0.3];
        assert!(matches!(
            resample(&samples, 0, 48000).unwrap(),
            Cow::Borrowed(_)
        ));
        assert!(matches!(
            resample(&samples, 48000, 0).unwrap(),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn test_resample_downsample_3x() {
        // 48kHz → 16kHz: should produce ~1/3 the samples
        let samples: Vec<f32> = (0..48000).map(|i| (i as f32) / 48000.0).collect();
        let result = resample(&samples, 48000, 16000).unwrap();
        assert!(matches!(result, Cow::Owned(_)));
        // Allow some tolerance for resampler edge effects
        let expected = 16000;
        assert!(
            (result.len() as i32 - expected).abs() < 100,
            "expected ~{expected} samples, got {}",
            result.len()
        );
    }

    #[test]
    fn test_resample_preserves_dc() {
        // Constant signal should remain constant after resampling
        let samples = vec![0.5f32; 48000];
        let result = resample(&samples, 48000, 16000).unwrap();
        // Skip edges (resampler startup/shutdown transients)
        let interior = &result[100..result.len().saturating_sub(100)];
        for &s in interior {
            assert!((s - 0.5).abs() < 0.01, "expected ~0.5, got {s}");
        }
    }

    #[test]
    fn test_resample_upsample() {
        // 16kHz → 48kHz: use 1s of audio so sinc filter edge effects are negligible
        let samples: Vec<f32> = (0..16000).map(|i| (i as f32) / 16000.0).collect();
        let result = resample(&samples, 16000, 48000).unwrap();
        assert!(matches!(result, Cow::Owned(_)));
        let expected = 48000;
        assert!(
            (result.len() as i32 - expected).abs() < 500,
            "expected ~{expected} samples, got {}",
            result.len()
        );
    }

    #[test]
    fn test_mono_passthrough() {
        let samples = vec![0.1, 0.2, 0.3];
        let result = deinterleave_to_mono(&samples, 1);
        assert!(matches!(result, Cow::Borrowed(_)), "mono should borrow");
        assert_eq!(&*result, &samples[..]);
    }

    #[test]
    fn test_stereo_to_mono() {
        let stereo = vec![0.8, 0.2, -0.4, 0.6];
        let mono = deinterleave_to_mono(&stereo, 2);
        assert!(matches!(mono, Cow::Owned(_)), "stereo should allocate");
        assert_eq!(mono.len(), 2);
        assert!((mono[0] - 0.5).abs() < f32::EPSILON);
        assert!((mono[1] - 0.1).abs() < f32::EPSILON);
    }

    #[test]
    fn test_empty() {
        let result = deinterleave_to_mono(&[], 2);
        assert!(result.is_empty());
    }

    #[test]
    fn test_zero_channels_passthrough() {
        // channels <= 1 triggers the passthrough path (includes 0)
        let samples = vec![0.5, -0.5];
        let result = deinterleave_to_mono(&samples, 0);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(&*result, &samples[..]);
    }

    #[test]
    fn test_quad_channel() {
        // 4 channels: [c0, c1, c2, c3] per frame
        let quad = vec![0.4, 0.8, 0.0, -0.4];
        let mono = deinterleave_to_mono(&quad, 4);
        assert_eq!(mono.len(), 1);
        // (0.4 + 0.8 + 0.0 + -0.4) / 4 = 0.2
        assert!((mono[0] - 0.2).abs() < f32::EPSILON);
    }

    #[test]
    fn test_incomplete_frame_truncates() {
        // 5 samples with 2 channels: the trailing sample is dropped
        let samples = vec![1.0, 0.0, 0.5, 0.5, 0.3];
        let mono = deinterleave_to_mono(&samples, 2);
        assert_eq!(mono.len(), 2);
        assert!((mono[0] - 0.5).abs() < f32::EPSILON);
        assert!((mono[1] - 0.5).abs() < f32::EPSILON);
    }
}
