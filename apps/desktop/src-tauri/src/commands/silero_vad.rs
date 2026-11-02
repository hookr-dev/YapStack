//! Silero VAD wrapper for the live-transcription loop.
//!
//! Replaces the hand-rolled RMS energy detector (`peek_energy_rms`) with a
//! neural speech detector. Silero is trained on speech specifically, so it
//! doesn't false-trigger on music, keyboard clicks, fans, or HVAC, and it
//! catches quiet / distant speech that sits below an RMS threshold.
//!
//! Used by every live session. The only things that stay engine-specific
//! are the *timing* knobs (silence_duration,
//! pre_roll, poll_interval, max_chunk_duration) in `VadTuning` — those
//! are independent of what produces the speech signal.
//!
//! Performance: Silero V5 inference on CPU is ~1 ms per 32 ms frame on an
//! Apple-Silicon core, well under our 100 ms Parakeet poll interval. The
//! model is bundled in the binary via `silero::BUNDLED_MODEL` (≈2 MB).

use silero::{SampleRate, Session, SessionOptions, StreamState};
use tracing::warn;
use yapstack_common::audio::resample;

/// Speech-probability threshold at or above which a frame is considered
/// speech (Silero's V5 default, validated against the upstream Python
/// reference in the crate's own tests).
pub(super) const SPEECH_THRESHOLD: f32 = 0.5;

/// End-of-speech threshold; paired with `SPEECH_THRESHOLD` to give
/// hysteresis and prevent flapping between states on short dips.
pub(super) const SILENCE_THRESHOLD: f32 = 0.35;

/// Target sample rate for Silero VAD. The model only supports 8 kHz and
/// 16 kHz; we use 16 kHz for better accuracy.
const TARGET_SAMPLE_RATE_HZ: u32 = 16_000;

/// Shared ONNX session. A single `Session` can feed multiple per-source
/// `StreamState`s sequentially (the session is `Send` but not `Sync`, so
/// callers must hold a mutable reference across a single call).
pub(super) struct SileroVad {
    session: Session,
}

impl SileroVad {
    /// Load the bundled Silero V5 model with default graph-optimization
    /// settings.
    pub(super) fn new() -> Result<Self, silero::Error> {
        let options = SessionOptions::default();
        let session = Session::bundled_with_options(options)?;
        Ok(Self { session })
    }

    /// Feed new audio samples (at `source_sample_rate`) into `stream`'s
    /// rolling Silero state and return *every* speech probability produced
    /// during the call, in emission order. Returns an empty vec if no full
    /// 32 ms frame landed yet (remaining samples stay buffered inside the
    /// stream state for the next call).
    ///
    /// The live loop needs all frames — not just the last — because a
    /// poll window can straddle a complete speech event. Even a 100 ms
    /// Parakeet poll can contain an utterance that
    /// starts and ends inside the batch; if we returned only the trailing
    /// probability (silence), the VAD state machine would never see the
    /// speech onset.
    pub(super) fn score_stream(
        &mut self,
        stream: &mut StreamState,
        samples: &[f32],
        source_sample_rate: u32,
    ) -> Vec<f32> {
        if samples.is_empty() {
            return Vec::new();
        }
        let resampled = match resample(samples, source_sample_rate, TARGET_SAMPLE_RATE_HZ) {
            Ok(r) => r,
            Err(e) => {
                warn!("silero resample failed: {e}");
                return Vec::new();
            }
        };
        if resampled.is_empty() {
            return Vec::new();
        }
        let mut probs: Vec<f32> = Vec::new();
        if let Err(e) = self
            .session
            .process_stream(stream, &resampled, |p| probs.push(p))
        {
            // Don't crash the live loop on a transient inference error —
            // the next poll tick will retry with fresh audio.
            warn!("silero inference failed: {e}");
            return Vec::new();
        }
        probs
    }

    /// Batch-score a full historical buffer (backfill use case). Returns
    /// one probability per 32 ms Silero frame (512 samples at 16 kHz).
    ///
    /// Creates a fresh `StreamState` internally — this is the right shape
    /// for backfill where we process a standalone window with no prior
    /// context. For live streaming use `score_stream` with a caller-owned
    /// `StreamState` so the LSTM memory persists across polls.
    pub(super) fn score_all(&mut self, samples: &[f32], source_sample_rate: u32) -> Vec<f32> {
        if samples.is_empty() {
            return Vec::new();
        }
        let resampled = match resample(samples, source_sample_rate, TARGET_SAMPLE_RATE_HZ) {
            Ok(r) => r,
            Err(e) => {
                warn!("silero batch resample failed: {e}");
                return Vec::new();
            }
        };
        if resampled.is_empty() {
            return Vec::new();
        }
        let mut stream = StreamState::new(SampleRate::Rate16k);
        let mut probs: Vec<f32> = Vec::new();
        if let Err(e) = self
            .session
            .process_stream(&mut stream, &resampled, |p| probs.push(p))
        {
            warn!("silero batch inference failed: {e}");
        }
        probs
    }
}

/// Silero V5 at 16 kHz emits one probability per 512-sample chunk
/// (≈32 ms). Exposed so callers mapping frame indices back to original
/// sample positions (e.g. backfill chunk boundaries) share the constant.
pub(super) const FRAME_DURATION_SECS: f32 = 0.032;

/// Per-source streaming state. One instance per audio source (mic, system).
pub(super) struct SileroSource {
    pub(super) stream: StreamState,
    /// Ring-buffer read cursor for VAD-only audio consumption. Independent
    /// of the chunk-extraction cursors (`vad.cursor`, `vad.speech_start_pos`)
    /// so reading for VAD doesn't disturb chunk boundaries.
    pub(super) read_pos: usize,
    /// Most recent probability produced by Silero. Sticks between polls
    /// that don't accumulate a full 32 ms frame so `poll_vad` keeps
    /// seeing a value rather than toggling to `None`.
    pub(super) last_probability: Option<f32>,
}

impl SileroSource {
    pub(super) fn new(initial_read_pos: usize) -> Self {
        Self {
            stream: StreamState::new(SampleRate::Rate16k),
            read_pos: initial_read_pos,
            last_probability: None,
        }
    }

    /// Reset the recurrent state. Called when the live loop jumps the read
    /// cursor (e.g. after backfill extraction) so the next poll starts with a
    /// clean LSTM context and no stale `last_probability`.
    pub(super) fn reset(&mut self) {
        self.stream.reset();
        self.last_probability = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silero_session_loads_and_returns_probability_for_silence() {
        // Smoke test: bundled model loads and produces a valid probability.
        let Ok(mut vad) = SileroVad::new() else {
            // ort runtime not available on this host (e.g. musl CI builder).
            // Mark the test as a soft skip by returning; we can't conditionally
            // #[ignore] at runtime, but the bundled session is expected to
            // work on every supported dev platform.
            eprintln!("skipping: Silero session init failed (ort unavailable)");
            return;
        };
        let mut source = SileroSource::new(0);
        // 100 ms of silence at 16 kHz = 1600 samples → 3 Silero frames (3 × 512).
        let silence = vec![0.0f32; 1_600];
        let probs = vad.score_stream(&mut source.stream, &silence, 16_000);
        assert!(
            !probs.is_empty(),
            "expected >=1 probability for 100ms input"
        );
        for p in &probs {
            assert!(
                (0.0..=1.0).contains(p),
                "probability must be in [0,1], got {p}"
            );
            assert!(
                *p < SPEECH_THRESHOLD,
                "pure silence should not cross speech threshold; got {p}"
            );
        }
    }

    #[test]
    fn silero_emits_one_prob_per_32ms_frame() {
        // Guard the frame cadence — Silero V5 at 16 kHz emits one prob per
        // 512-sample (32 ms) chunk. We rely on this in the live loop to
        // feed each frame into the VAD state machine separately.
        let Ok(mut vad) = SileroVad::new() else {
            eprintln!("skipping: Silero session init failed (ort unavailable)");
            return;
        };
        let mut source = SileroSource::new(0);
        // Exactly 5 frames (5 × 512 = 2560 samples at 16 kHz = 160 ms).
        let silence = vec![0.0f32; 512 * 5];
        let probs = vad.score_stream(&mut source.stream, &silence, 16_000);
        assert_eq!(
            probs.len(),
            5,
            "expected 5 probs for 5 × 512 samples; got {}",
            probs.len()
        );
    }
}
