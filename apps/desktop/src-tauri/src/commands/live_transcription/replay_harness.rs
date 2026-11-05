//! Replay WAVs through the app's own chunking and write the chunks out, so
//! `tools/asr-bench replay` can score engines on the chunks users' audio is
//! actually cut into. `#[ignore]`d: it needs a corpus, and the bench drives
//! it with `--ignored`.
//!
//! - `YAPSTACK_REPLAY_LIST`: a file with one WAV path per line (PCM16 or
//!   float, any rate and channel count; mixed down to mono as capture is).
//! - `YAPSTACK_REPLAY_OUT`: where the chunk WAVs and `manifest.jsonl` go.
//! - `YAPSTACK_REPLAY_MODE`:
//!   - `live`: `backfill_chunks_from_probabilities` over the whole clip: the
//!     frame-based state machine backfill uses to match live segmentation
//!     (same tuning, hysteresis, pre-roll and pause-search cuts). It is not
//!     the polled live loop (`poll_vad`) itself, which times silence and
//!     chunk length by wall clock.
//!   - `backfill`: `vad_chunk_historical_audio`, the backfill path (VAD
//!     chunks, then the job-quantum split).
//!   - `rewind`: a session started with a rewind while someone was talking.
//!     The backfill buffer ends at the first confident speech frame after
//!     the clip's midpoint. `carry_open_tail_into_live` runs on a real
//!     `SourceVadState` and decides how much backfill keeps; the live side
//!     is the same frame-based state machine from the seam on, started
//!     speaking at the carried start with the carried audio already counted
//!     towards the max chunk length. One difference from the app: a length
//!     cut in the first live chunk searches only frames after the seam,
//!     where the app's `recent_frames` also holds the carried ones.

use super::*;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use yapstack_common::types::EngineKind;

fn read_mono(path: &str) -> (Vec<f32>, u32) {
    let mut reader = hound::WavReader::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    let spec = reader.spec();
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let scale = (1u32 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.expect("sample") as f32 / scale)
                .collect()
        }
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .map(|s| s.expect("sample"))
            .collect(),
    };
    let mono = yapstack_common::audio::deinterleave_to_mono(&interleaved, spec.channels);
    (mono.into_owned(), spec.sample_rate)
}

fn offset(chunks: Vec<VadBackfillChunk>, by: usize) -> Vec<VadBackfillChunk> {
    chunks
        .into_iter()
        .map(|c| VadBackfillChunk {
            start: c.start + by,
            end: c.end + by,
        })
        .collect()
}

/// Live chunks from `seam` on, for a mono source whose cursor is at `seam`
/// and which `carry_open_tail_into_live` may have left speaking: the walk
/// then starts mid-utterance at the carried start, with the carried audio
/// already counted towards the max chunk length.
fn live_chunks_after_carry(
    source: &SourceVadState,
    seam: usize,
    live_probs: &[f32],
    live_len: usize,
    sr: u32,
    tuning: &VadTuning,
) -> Vec<VadBackfillChunk> {
    let open = source.is_speaking.then(|| OpenSpeech {
        start: 0,
        frames: ((seam - source.speech_start_pos) as f32
            / sr as f32
            / super::super::silero_vad::FRAME_DURATION_SECS) as usize,
    });
    let mut live = offset(
        vad_chunks_and_open_tail(live_probs, live_len, sr, tuning, open).0,
        seam,
    );
    if open.is_some() {
        if let Some(first) = live.first_mut() {
            first.start = source.speech_start_pos;
        }
    }
    live
}

fn rewind_chunks(mono: &[f32], sr: u32, tuning: &VadTuning) -> Vec<VadBackfillChunk> {
    use super::super::silero_vad::frame_start_sample;
    let mut silero = SileroVad::new().expect("silero");
    let probs = silero.score_all(mono, sr);
    let half = probs.len() / 2;
    let onset = (half..probs.len())
        .find(|&k| probs[k] >= 0.9)
        .unwrap_or(half);
    // A few samples into the speech frame, so the seam lands mid-speech.
    let seam = (frame_start_sample(onset, sr) + 300).min(mono.len());

    // As session start does it: score the backfill's tail, hand an open
    // utterance to the live source, and truncate the backfill there.
    let tail_offset = seam.saturating_sub((BACKFILL_SEAM_SCORE_SECS * sr as f32) as usize);
    let tail_probs = silero.score_all(&mono[tail_offset..seam], sr);
    let mut source = SourceVadState::new(AudioSourceLabel::Mic, seam, 0, sr, 1, 0.0);
    let keep = carry_open_tail_into_live(&mut source, seam, tail_offset, &tail_probs, sr, tuning);

    let mut chunks = if keep > 0 {
        vad_chunk_historical_audio(&mono[..keep], sr, tuning)
    } else {
        Vec::new()
    };
    let live_probs = silero.score_all(&mono[seam..], sr);
    chunks.extend(live_chunks_after_carry(
        &source,
        seam,
        &live_probs,
        mono.len() - seam,
        sr,
        tuning,
    ));
    chunks
}

#[test]
#[ignore = "needs YAPSTACK_REPLAY_LIST and YAPSTACK_REPLAY_OUT; run by tools/asr-bench replay"]
fn replay_chunk_boundaries() {
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
    let list = var("YAPSTACK_REPLAY_LIST");
    let out = PathBuf::from(var("YAPSTACK_REPLAY_OUT"));
    let mode = std::env::var("YAPSTACK_REPLAY_MODE").unwrap_or_else(|_| "live".into());
    std::fs::create_dir_all(&out).expect("create out dir");
    let tuning = profile_for(EngineKind::Parakeet).vad_tuning;
    let mut manifest =
        std::fs::File::create(out.join("manifest.jsonl")).expect("create manifest.jsonl");

    for wav in std::fs::read_to_string(&list).expect("read list").lines() {
        let wav = wav.trim();
        if wav.is_empty() {
            continue;
        }
        let (mono, sr) = read_mono(wav);
        let chunks = match mode.as_str() {
            "live" => {
                let probs = SileroVad::new().expect("silero").score_all(&mono, sr);
                backfill_chunks_from_probabilities(&probs, mono.len(), sr, &tuning)
            }
            "backfill" => vad_chunk_historical_audio(&mono, sr, &tuning),
            "rewind" => rewind_chunks(&mono, sr, &tuning),
            other => panic!("YAPSTACK_REPLAY_MODE={other:?}: want live, backfill or rewind"),
        };
        let stem = Path::new(wav)
            .file_stem()
            .expect("wav file name")
            .to_string_lossy()
            .into_owned();
        for (i, c) in chunks.iter().enumerate() {
            let path = out.join(format!("{stem}__{i:03}.wav"));
            yapstack_audio::export::write_wav(&mono[c.start..c.end], sr, 1, &path)
                .expect("write chunk");
            let row = serde_json::json!({
                "clip": wav,
                "idx": i,
                "start": c.start,
                "end": c.end,
                "sr": sr,
                "path": path,
            });
            writeln!(manifest, "{row}").expect("write manifest");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rewind seam 8 s into an utterance, then 6 s of speech the live
    /// Silero stream scores between the offset and onset thresholds, then
    /// silence. The live source the carry leaves is already speaking, so
    /// the app keeps that speech, cuts the carried chunk for length about
    /// 10 s after its start, and makes no chunk boundary at the seam.
    #[test]
    fn live_after_a_carry_continues_the_open_utterance() {
        let sr = 16_000u32;
        let tuning = profile_for(EngineKind::Parakeet).vad_tuning;
        let frame = 512usize;
        let tail_offset = 5 * sr as usize;
        let seam = 20 * sr as usize;
        let (silent, speech) = (218usize, 250usize);
        let mut tail_probs = vec![0.1f32; silent];
        tail_probs.extend(std::iter::repeat_n(0.9, speech));
        let mut source = SourceVadState::new(AudioSourceLabel::Mic, seam, 0, sr, 1, 0.0);
        let keep =
            carry_open_tail_into_live(&mut source, seam, tail_offset, &tail_probs, sr, &tuning);
        assert!(keep < seam && source.is_speaking);

        let mut live_probs = vec![0.4f32; 6 * 1000 / 32];
        live_probs.extend(std::iter::repeat_n(0.1, 2 * 1000 / 32));
        let live_len = live_probs.len() * frame;
        let chunks = live_chunks_after_carry(&source, seam, &live_probs, live_len, sr, &tuning);

        let max = tuning.max_chunk_duration.as_secs_f32() * sr as f32;
        let first = chunks[0];
        assert_eq!(first.start, keep);
        assert!(
            (first.end - first.start) as f32 <= max + frame as f32,
            "{chunks:?}"
        );
        assert!(first.end > seam + sr as usize, "{chunks:?}");
        for pair in chunks.windows(2) {
            assert_eq!(pair[0].end, pair[1].start, "{chunks:?}");
        }
        assert!(
            chunks.last().unwrap().end >= seam + 6 * sr as usize,
            "{chunks:?}"
        );
    }

    #[test]
    fn open_speech_seed_starts_the_walk_speaking() {
        let sr = 16_000u32;
        let tuning = profile_for(EngineKind::Parakeet).vad_tuning;
        let probs = vec![0.4f32; 40];
        let (none, still_open) = vad_chunks_and_open_tail(&probs, 40 * 512, sr, &tuning, None);
        assert!(none.is_empty() && !still_open);
        let seed = OpenSpeech {
            start: 0,
            frames: 0,
        };
        let (seeded, still_open) =
            vad_chunks_and_open_tail(&probs, 40 * 512, sr, &tuning, Some(seed));
        assert!(still_open);
        assert_eq!(
            seeded,
            vec![VadBackfillChunk {
                start: 0,
                end: 40 * 512
            }]
        );
    }
}
