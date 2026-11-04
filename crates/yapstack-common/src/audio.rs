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
/// `yapstack-audio::mixer` call through here. A caller that resamples many
/// buffers at one rate pair keeps a [`MonoResampler`] instead.
pub fn resample(
    samples: &[f32],
    from_rate: u32,
    to_rate: u32,
) -> Result<std::borrow::Cow<'_, [f32]>, ResampleError> {
    use rubato::{Resampler, SincFixedIn};

    if from_rate == 0 || to_rate == 0 || samples.is_empty() || from_rate == to_rate {
        return Ok(std::borrow::Cow::Borrowed(samples));
    }

    let ratio = to_rate as f64 / from_rate as f64;
    // chunk_size: process in blocks; use full input length for single-shot
    let chunk_size = samples.len();

    let mut resampler =
        SincFixedIn::<f32>::new(ratio, 2.0, sinc_params(), chunk_size, 1).map_err(|e| {
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

fn sinc_params() -> rubato::SincInterpolationParameters {
    use rubato::{SincInterpolationParameters, SincInterpolationType, WindowFunction};
    SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.98,
        interpolation: SincInterpolationType::Cubic,
        oversampling_factor: 128,
        window: WindowFunction::BlackmanHarris2,
    }
}

/// Input frames per `SincFixedIn` call. Fixed so one resampler serves
/// buffers of any length, and so its scratch buffers stay small.
const RESAMPLE_BLOCK_FRAMES: usize = 1024;

/// [`resample`]'s filter for one rate pair, built once and reused: each
/// [`MonoResampler::process`] call is independent (state is reset first).
/// Unlike the single-shot [`resample`], which drops the filter tail, the
/// output is exactly `round(len * to_rate / from_rate)` samples long.
pub struct MonoResampler {
    from_rate: u32,
    to_rate: u32,
    inner: rubato::SincFixedIn<f32>,
    out_block: Vec<Vec<f32>>,
}

impl MonoResampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Result<Self, ResampleError> {
        use rubato::{Resampler, SincFixedIn};
        let ratio = to_rate as f64 / from_rate as f64;
        let inner = SincFixedIn::<f32>::new(ratio, 1.0, sinc_params(), RESAMPLE_BLOCK_FRAMES, 1)
            .map_err(|e| ResampleError::InitFailed {
                ratio,
                chunk_size: RESAMPLE_BLOCK_FRAMES,
                reason: e.to_string(),
            })?;
        let out_block = inner.output_buffer_allocate(true);
        Ok(Self {
            from_rate,
            to_rate,
            inner,
            out_block,
        })
    }

    pub fn rates(&self) -> (u32, u32) {
        (self.from_rate, self.to_rate)
    }

    pub fn process(&mut self, samples: &[f32]) -> Result<Vec<f32>, ResampleError> {
        use rubato::Resampler;
        let ratio = self.to_rate as f64 / self.from_rate as f64;
        let fail = |e: rubato::ResampleError| ResampleError::ProcessFailed {
            ratio,
            sample_count: samples.len(),
            reason: e.to_string(),
        };
        self.inner.reset();
        let expected = (samples.len() as f64 * ratio).round() as usize;
        let mut out = Vec::with_capacity(expected + self.inner.output_frames_max());

        let (blocks, remainder) = samples.as_chunks::<RESAMPLE_BLOCK_FRAMES>();
        for block in blocks {
            let (_, n) = self
                .inner
                .process_into_buffer(&[block.as_slice()], &mut self.out_block, None)
                .map_err(fail)?;
            out.extend_from_slice(&self.out_block[0][..n]);
        }
        // rubato rejects an empty partial block, which a whole number of
        // blocks would leave.
        if !remainder.is_empty() {
            let (_, n) = self
                .inner
                .process_partial_into_buffer(Some(&[remainder]), &mut self.out_block, None)
                .map_err(fail)?;
            out.extend_from_slice(&self.out_block[0][..n]);
        }
        // rubato starts its first output at input frame 0 (its initial index
        // already offsets the filter), so only the tail needs flushing.
        while out.len() < expected {
            let (_, n) = self
                .inner
                .process_partial_into_buffer(None::<&[&[f32]]>, &mut self.out_block, None)
                .map_err(fail)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&self.out_block[0][..n]);
        }
        out.truncate(expected);
        Ok(out)
    }
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

    fn sine(rate: u32, hz: f32, secs: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| (std::f32::consts::TAU * hz * i as f32 / rate as f32).sin())
            .collect()
    }

    fn mono_resample(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
        MonoResampler::new(from, to)
            .unwrap()
            .process(samples)
            .unwrap()
    }

    #[test]
    fn mono_resampler_length_is_exact() {
        for (from, len) in [
            (48_000, 48_000),
            (44_100, 44_100),
            (48_000, 1000),
            (48_000, 3),
        ] {
            let out = mono_resample(&vec![0.1; len], from, 16_000);
            let expected = (len as f64 * 16_000.0 / from as f64).round() as usize;
            assert_eq!(out.len(), expected, "{from} Hz, {len} samples");
        }
    }

    /// Same filter, same timeline as the single-shot [`resample`]: the
    /// block-wise output matches it sample for sample, and only adds the
    /// filter tail that [`resample`] drops.
    #[test]
    fn mono_resampler_matches_single_shot_resample() {
        for (from, input) in [
            (48_000, sine(48_000, 440.0, 0.5)),
            (44_100, sine(44_100, 1000.0, 0.3)),
        ] {
            let single = resample(&input, from, 16_000).unwrap();
            let blocks = mono_resample(&input, from, 16_000);
            assert!(blocks.len() > single.len());
            let max_diff = single
                .iter()
                .zip(&blocks)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0_f32, f32::max);
            assert!(max_diff < 1e-4, "{from} Hz: max diff {max_diff}");
        }
    }

    #[test]
    fn reused_resampler_matches_a_fresh_one() {
        let a = sine(48_000, 300.0, 0.7);
        let b = sine(48_000, 1000.0, 0.2);
        let mut reused = MonoResampler::new(48_000, 16_000).unwrap();
        assert_eq!(reused.rates(), (48_000, 16_000));
        let first = reused.process(&a).unwrap();
        let second = reused.process(&b).unwrap();
        assert_eq!(first, mono_resample(&a, 48_000, 16_000));
        assert_eq!(second, mono_resample(&b, 48_000, 16_000));
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
