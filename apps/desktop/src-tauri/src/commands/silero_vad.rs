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

use std::collections::VecDeque;

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

/// Samples per Silero frame at `TARGET_SAMPLE_RATE_HZ`.
const FRAME_SAMPLES: usize = 512;

/// Where frame `frame` of a `score_all` stream starts in the scored buffer
/// at `sample_rate`: the first sample at or after the frame's start time.
/// Exact for every rate, including those (44.1 kHz) whose frames aren't a
/// whole number of samples, and consistent with `frame_at_sample`.
pub(super) fn frame_start_sample(frame: usize, sample_rate: u32) -> usize {
    (frame as u64 * FRAME_SAMPLES as u64 * sample_rate as u64)
        .div_ceil(TARGET_SAMPLE_RATE_HZ as u64) as usize
}

/// The frame of a `score_all` stream whose audio holds sample `pos` of the
/// scored buffer at `sample_rate`.
pub(super) fn frame_at_sample(pos: usize, sample_rate: u32) -> usize {
    (pos as u64 * TARGET_SAMPLE_RATE_HZ as u64 / (FRAME_SAMPLES as u64 * sample_rate as u64))
        as usize
}

/// How many recent frames `SileroSource` keeps (~33 s), enough to reach
/// back past a drain backlog to where the next chunk would be cut.
const RECENT_FRAME_CAPACITY: usize = 1024;

/// Index of the lowest probability among the frames that make up the last
/// `window_ms` of `probs` (each frame `frame_ms` long). Ties go to the latest
/// frame. `None` when `probs` is empty.
pub(super) fn trailing_minimum_frame(
    probs: &[f32],
    frame_ms: f32,
    window_ms: f32,
) -> Option<usize> {
    if probs.is_empty() {
        return None;
    }
    let window = ((window_ms / frame_ms) as usize).clamp(1, probs.len());
    let first = probs.len() - window;
    let mut best = first;
    for (i, &p) in probs.iter().enumerate().skip(first) {
        if p <= probs[best] {
            best = i;
        }
    }
    Some(best)
}

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
    /// The most recent frame probabilities, oldest first, each paired with
    /// the ring-buffer position of the middle of that frame's audio.
    pub(super) recent_frames: VecDeque<(usize, f32)>,
}

impl SileroSource {
    pub(super) fn new(initial_read_pos: usize) -> Self {
        Self {
            stream: StreamState::new(SampleRate::Rate16k),
            read_pos: initial_read_pos,
            last_probability: None,
            recent_frames: VecDeque::new(),
        }
    }

    /// Remember `probs`, the frames one `score_stream` call just produced.
    /// The audio fed so far ends at ring-buffer position `end_pos`, and the
    /// stream's pending (not yet scored) samples are the last of it.
    /// Positions are interleaved, so one 16 kHz sample spans
    /// `source_sample_rate * channels / 16 000` of them.
    pub(super) fn record_frames(
        &mut self,
        probs: &[f32],
        end_pos: usize,
        source_sample_rate: u32,
        channels: u16,
    ) {
        let positions_per_sample =
            source_sample_rate as f64 * channels.max(1) as f64 / TARGET_SAMPLE_RATE_HZ as f64;
        let pending = self.stream.pending_len() as f64;
        for (i, &p) in probs.iter().enumerate() {
            let frames_after = (probs.len() - 1 - i) as f64;
            let back = pending + frames_after * FRAME_SAMPLES as f64 + FRAME_SAMPLES as f64 / 2.0;
            let mid = end_pos.saturating_sub((back * positions_per_sample).round() as usize);
            if self.recent_frames.len() == RECENT_FRAME_CAPACITY {
                self.recent_frames.pop_front();
            }
            self.recent_frames.push_back((mid, p));
        }
    }

    /// Reset the recurrent state. Called when the live loop jumps the read
    /// cursor (e.g. after backfill extraction) so the next poll starts with a
    /// clean LSTM context and no stale `last_probability`.
    pub(super) fn reset(&mut self) {
        self.stream.reset();
        self.last_probability = None;
        self.recent_frames.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_MS: f32 = FRAME_DURATION_SECS * 1000.0;

    /// 12 s of steady speech with one quiet frame at ~9.2 s: the lowest
    /// frame in the 2 s before a 10 s cap is the dip.
    #[test]
    fn trailing_minimum_frame_finds_the_dip_before_the_cap() {
        let dip = (9.2 / FRAME_DURATION_SECS) as usize;
        let cap = (10.0 / FRAME_DURATION_SECS) as usize;
        let mut probs = vec![0.9f32; (12.0 / FRAME_DURATION_SECS) as usize];
        probs[dip] = 0.1;
        assert_eq!(
            trailing_minimum_frame(&probs[..cap], FRAME_MS, 2000.0),
            Some(dip)
        );
    }

    #[test]
    fn trailing_minimum_frame_ignores_frames_before_the_window() {
        let mut probs = vec![0.9f32; 200];
        probs[10] = 0.0;
        probs[150] = 0.5;
        assert_eq!(trailing_minimum_frame(&probs, FRAME_MS, 2000.0), Some(150));
    }

    #[test]
    fn trailing_minimum_frame_breaks_ties_toward_the_latest_frame() {
        assert_eq!(
            trailing_minimum_frame(&[0.9; 100], FRAME_MS, 2000.0),
            Some(99)
        );
        assert_eq!(
            trailing_minimum_frame(&[0.2, 0.1, 0.1, 0.3], FRAME_MS, 2000.0),
            Some(2)
        );
        assert_eq!(trailing_minimum_frame(&[], FRAME_MS, 2000.0), None);
    }

    /// Frames scored across several polls get positions at the middle of
    /// their own audio: 48 kHz stereo spans 6 positions per 16 kHz sample.
    #[test]
    fn record_frames_places_each_frame_at_its_audio_midpoint() {
        let mut source = SileroSource::new(0);
        let per_frame = FRAME_SAMPLES * 6;
        source.record_frames(&[0.1, 0.2], 2 * per_frame, 48_000, 2);
        source.record_frames(&[0.3], 3 * per_frame, 48_000, 2);
        let mids: Vec<usize> = source.recent_frames.iter().map(|&(m, _)| m).collect();
        assert_eq!(
            mids,
            vec![per_frame / 2, 3 * per_frame / 2, 5 * per_frame / 2]
        );
        source.reset();
        assert!(source.recent_frames.is_empty());
    }

    #[test]
    fn frame_sample_mapping_is_exact_and_consistent() {
        assert_eq!(frame_start_sample(10, 16_000), 5_120);
        assert_eq!(frame_start_sample(10, 48_000), 15_360);
        // 9375 frames = 300 s; 1411.2 samples each at 44.1 kHz.
        assert_eq!(frame_start_sample(9_375, 44_100), 13_230_000);
        for sr in [16_000, 22_050, 44_100, 48_000] {
            for k in [0, 1, 2, 7, 999, 9_374] {
                let s = frame_start_sample(k, sr);
                assert_eq!(frame_at_sample(s, sr), k, "{sr} {k}");
                assert_eq!(
                    frame_at_sample(s.saturating_sub(1), sr),
                    k.saturating_sub(1)
                );
            }
        }
    }

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
