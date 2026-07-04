//! Parakeet TDT-v3 backend (parakeet-rs 0.3 + ONNX Runtime via `ort`).

use std::path::{Path, PathBuf};

use parakeet_rs::sortformer::{Sortformer, SpeakerSegment};
use parakeet_rs::{ParakeetTDT, TimedToken, TimestampMode, Transcriber};
use tracing::{debug, info, warn};
use yapstack_common::types::TranscriptSegment;

use crate::engines::{
    normalize_spacing, read_wav_as_mono_16k, sanitize_text, should_include_segment, EngineInfo,
    TranscribeOpts, TranscriptionBackend, TranscriptionOutput,
};

const SORTFORMER_SAMPLE_RATE: u32 = 16000;
/// parakeet-rs's `transcribe_samples` rejects anything other than 16 kHz mono
/// despite docs claiming auto-resampling (verified against parakeet-rs 0.3.5).
const PARAKEET_SAMPLE_RATE: u32 = 16000;

const SEGMENT_GAP_SECS: f32 = 0.5;
const MAX_SEGMENT_SECS: f32 = 12.0;

pub struct ParakeetBackend {
    model: Option<ParakeetTDT>,
    sortformer_model_path: Option<PathBuf>,
    sortformer: Option<Sortformer>,
    coreml_cache_dir: Option<PathBuf>,
    /// Set after each successful `load_model` so `engine_info()` can
    /// report what's actually running. Cleared if reload fails.
    last_engine_info: Option<EngineInfo>,
}

impl ParakeetBackend {
    pub fn new(sortformer_model_path: Option<PathBuf>, coreml_cache_dir: Option<PathBuf>) -> Self {
        Self {
            model: None,
            sortformer_model_path,
            sortformer: None,
            coreml_cache_dir,
            last_engine_info: None,
        }
    }

    fn ensure_sortformer(&mut self) -> Result<&mut Sortformer, String> {
        if self.sortformer.is_none() {
            let path = self
                .sortformer_model_path
                .as_ref()
                .ok_or("sortformer model not provided to sidecar")?;
            info!(
                "loading sortformer diarization model from {}",
                path.display()
            );
            let s = Sortformer::new(path)
                .map_err(|e| format!("failed to load sortformer model: {e}"))?;
            self.sortformer = Some(s);
        }
        Ok(self.sortformer.as_mut().unwrap())
    }
}

impl TranscriptionBackend for ParakeetBackend {
    fn load_model(&mut self, model_path: &Path) -> Result<(), String> {
        if !model_path.is_dir() {
            return Err(format!(
                "parakeet model path must be a directory containing encoder-model.onnx and \
                 decoder_joint-model.onnx, got: {}",
                model_path.display()
            ));
        }
        let load_start = std::time::Instant::now();
        info!(
            "loading parakeet TDT model from {} (coreml_cache={:?})",
            model_path.display(),
            self.coreml_cache_dir.as_deref()
        );

        let cache_dir = self.coreml_cache_dir.as_deref();
        let choice = AccelChoice::from_env();
        // Auto: skip CoreML when external `.onnx.data` is present (known ORT
        // bug `model_path must not be empty`). The skip ONLY applies when
        // CoreML is the auto-selected EP — WebGPU runs through a different
        // ORT codepath that doesn't try to serialize the partitioned subgraph
        // back through the model path, so it's safe with external data.
        // User-forced choices are honored regardless (so a hand-set
        // `YAPSTACK_PARAKEET_ACCEL=coreml` will still try CoreML even on a
        // fp32 bundle, fail, and hit the fallback chain below).
        let exec_config = match choice {
            AccelChoice::Auto if auto_skip_due_to_external_data(model_path) => None,
            _ => build_exec_config(choice, cache_dir),
        };
        let accel_attempted = exec_config.is_some();
        let attempted_label = if accel_attempted {
            resolved_accel_label(choice)
        } else {
            "cpu"
        };
        info!(
            marker = "live_accel_choice",
            engine = "parakeet",
            requested = ?choice,
            attempted = attempted_label,
            model_dir = %model_path.display(),
            "parakeet acceleration resolved"
        );
        // `actual_label` records which EP actually took the model — it
        // diverges from `attempted_label` only on the fallback path below.
        let (model, actual_label) = match ParakeetTDT::from_pretrained(model_path, exec_config) {
            Ok(m) => (m, attempted_label.to_string()),
            Err(e) if accel_attempted => {
                warn!(
                    marker = "live_accel_fallback",
                    engine = "parakeet",
                    requested = attempted_label,
                    error = %e,
                    "accelerator load failed; falling back to CPU \
                     (set YAPSTACK_PARAKEET_ACCEL=cpu to suppress this attempt)"
                );
                let m = ParakeetTDT::from_pretrained(model_path, None)
                    .map_err(|e2| format!("failed to load parakeet model (CPU fallback): {e2}"))?;
                (m, "cpu".to_string())
            }
            Err(e) => return Err(format!("failed to load parakeet model: {e}")),
        };
        self.model = Some(model);
        self.last_engine_info = Some(EngineInfo {
            accel: actual_label,
            model_dir: model_path.display().to_string(),
        });
        info!(
            "parakeet TDT model loaded in {} ms",
            load_start.elapsed().as_millis()
        );
        Ok(())
    }

    fn engine_info(&self) -> Option<EngineInfo> {
        self.last_engine_info.clone()
    }

    fn transcribe(
        &mut self,
        audio_path: &Path,
        opts: TranscribeOpts<'_>,
    ) -> Result<TranscriptionOutput, String> {
        let model = self.model.as_mut().ok_or("no model loaded")?;

        let mono_16k = read_wav_as_mono_16k(audio_path)?;
        let audio_seconds = mono_16k.len() as f32 / PARAKEET_SAMPLE_RATE as f32;
        debug!(
            "parakeet transcribe: {} samples ({:.2}s) at 16kHz mono (diarization={})",
            mono_16k.len(),
            audio_seconds,
            opts.diarization
        );

        _ = opts.language;
        _ = opts.initial_prompt;
        _ = opts.single_segment;

        let start = std::time::Instant::now();
        let (transcribe_input, diarize_input) = if opts.diarization {
            (mono_16k.clone(), Some(mono_16k))
        } else {
            (mono_16k, None)
        };
        let result = model
            .transcribe_samples(
                transcribe_input,
                PARAKEET_SAMPLE_RATE,
                1,
                Some(TimestampMode::Words),
            )
            .map_err(|e| format!("parakeet transcription failed: {e}"))?;
        let elapsed_ms = start.elapsed().as_millis();
        let rtfx = if elapsed_ms > 0 {
            (audio_seconds * 1000.0) / elapsed_ms as f32
        } else {
            f32::INFINITY
        };
        info!(
            "parakeet transcribe: {} tokens in {} ms ({:.1}x realtime)",
            result.tokens.len(),
            elapsed_ms,
            rtfx
        );

        let segments = group_tokens_into_segments(&result.tokens);
        let mut text = String::new();
        let mut filtered: Vec<TranscriptSegment> = Vec::with_capacity(segments.len());
        for seg in segments {
            // parakeet-rs's `TimedToken` has no logprob/confidence field — the
            // library doesn't expose one. We pass 1.0 so the shared
            // `should_include_segment` contract still works, and accept the
            // consequence: the confidence-gated branches (marginal-reject at
            // < 0.6, repetition filter at < 0.7) never fire for Parakeet
            // output. Parakeet's narrower always-reject list was designed as
            // the sole filter precisely because Parakeet doesn't hallucinate
            // the YouTube artifacts Whisper does. If a future parakeet-rs
            // release exposes token probabilities we should route a derived
            // confidence here so the lower tiers start firing.
            let confidence = 1.0_f32;
            let normalized = normalize_spacing(seg.text.trim());
            let sanitized = sanitize_text(&normalized);
            if !should_include_segment(
                &sanitized,
                confidence,
                yapstack_common::types::EngineKind::Parakeet,
            ) {
                info!(len = sanitized.chars().count(), "parakeet segment filtered");
                continue;
            }
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&sanitized);
            filtered.push(TranscriptSegment {
                start_ms: (seg.start_secs * 1000.0).round() as u64,
                end_ms: (seg.end_secs * 1000.0).round() as u64,
                text: sanitized,
                confidence,
                speaker_id: None,
            });
        }

        if let Some(diarize_audio) = diarize_input {
            match self.run_diarization(diarize_audio) {
                Ok(speaker_segs) => {
                    debug!(
                        "sortformer produced {} speaker segments",
                        speaker_segs.len()
                    );
                    assign_speakers(&mut filtered, &speaker_segs);
                }
                Err(e) => {
                    warn!("diarization failed; returning transcript without speaker IDs: {e}");
                }
            }
        }

        let duration_ms = start.elapsed().as_millis() as u64;
        Ok(TranscriptionOutput {
            text,
            segments: filtered,
            duration_ms,
        })
    }
}

impl ParakeetBackend {
    /// Run Sortformer diarization on a single chunk's audio.
    ///
    /// **Known limitation — chunk-local speaker IDs.** Sortformer::diarize()
    /// resets internal state per call, so the `speaker_id` values returned
    /// here are only consistent *within this chunk*. The frontend treats
    /// speaker_id as a session-stable identity (groups + renames by id), so
    /// the same person can end up labeled Speaker 0 in one chunk and
    /// Speaker 1 in the next. This is why diarization is currently off by
    /// default in Settings and the UI falls back to source-based labels.
    ///
    /// Before re-enabling diarization for users, we need one of:
    ///   - Streaming Sortformer state across calls (parakeet-rs feature),
    ///   - A post-session pass that runs Sortformer once on the full WAV
    ///     and retro-annotates segments, or
    ///   - An embedding-based speaker registry that maps chunk-local IDs to
    ///     session-stable IDs via similarity matching.
    fn run_diarization(&mut self, mono_16k: Vec<f32>) -> Result<Vec<SpeakerSegment>, String> {
        let sortformer = self.ensure_sortformer()?;
        sortformer
            .diarize(mono_16k, SORTFORMER_SAMPLE_RATE, 1)
            .map_err(|e| format!("sortformer diarize failed: {e}"))
    }
}

fn has_external_data_file(model_dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(model_dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name.ends_with(".onnx.data") || name.ends_with(".onnx_data")
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccelChoice {
    Auto,
    Cpu,
    #[cfg(feature = "coreml")]
    CoreMl,
    #[cfg(feature = "webgpu")]
    WebGpu,
}

impl AccelChoice {
    fn from_env() -> Self {
        let raw = std::env::var("YAPSTACK_PARAKEET_ACCEL").unwrap_or_default();
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Self::Auto,
            "cpu" => Self::Cpu,
            #[cfg(feature = "coreml")]
            "coreml" => Self::CoreMl,
            #[cfg(not(feature = "coreml"))]
            "coreml" => {
                warn!("YAPSTACK_PARAKEET_ACCEL=coreml ignored: feature not compiled in");
                Self::Auto
            }
            #[cfg(feature = "webgpu")]
            "webgpu" => Self::WebGpu,
            #[cfg(not(feature = "webgpu"))]
            "webgpu" => {
                warn!("YAPSTACK_PARAKEET_ACCEL=webgpu ignored: feature not compiled in");
                Self::Auto
            }
            other => {
                warn!("YAPSTACK_PARAKEET_ACCEL={other:?} unrecognised; defaulting to Auto");
                Self::Auto
            }
        }
    }
}

/// Stable string label for the accelerator that an `AccelChoice` resolves
/// to under the current cfg. Matches what `auto_exec_config` actually
/// builds for `AccelChoice::Auto`. Used for the `ModelLoaded.accel`
/// telemetry payload — kept lowercase + ASCII for grep-friendliness.
fn resolved_accel_label(choice: AccelChoice) -> &'static str {
    match choice {
        AccelChoice::Cpu => "cpu",
        AccelChoice::Auto => {
            #[cfg(all(target_os = "macos", feature = "webgpu"))]
            {
                "webgpu"
            }
            #[cfg(all(target_os = "macos", not(feature = "webgpu"), feature = "coreml"))]
            {
                "coreml"
            }
            // Auto stays CPU on non-macOS until Gate C (canary validates WebGPU
            // correct + faster than CPU on real Windows GPU); then flip Auto ->
            // WebGPU. Current opt-in: YAPSTACK_PARAKEET_ACCEL=webgpu.
            #[cfg(not(target_os = "macos"))]
            {
                "cpu"
            }
            #[cfg(all(target_os = "macos", not(feature = "webgpu"), not(feature = "coreml")))]
            {
                "cpu"
            }
        }
        #[cfg(feature = "coreml")]
        AccelChoice::CoreMl => "coreml",
        #[cfg(feature = "webgpu")]
        AccelChoice::WebGpu => "webgpu",
    }
}

/// Returns true when the model dir contains an external `.onnx.data` file
/// AND the auto-selected accelerator would be a CoreML-style EP that
/// stumbles on the "model_path must not be empty" partitioning bug. With
/// the new Apple Silicon default of WebGPU, this only fires when CoreML
/// is the only EP compiled in (legacy/fallback build) — which means the
/// fp32 bundle on those builds keeps its CPU-only behavior.
//
// `needless_return` is allowed here for the same reason as `auto_exec_config`:
// the trailing arms are mutually-exclusive `#[cfg]` blocks (only one compiled
// per target × feature), and the early `return false` in the webgpu arm reads
// as a clean guard. Lint-only: no runtime behavior changes.
#[allow(clippy::needless_return)]
fn auto_skip_due_to_external_data(model_path: &Path) -> bool {
    if !has_external_data_file(model_path) {
        return false;
    }
    // If WebGPU is compiled in, Auto will pick WebGPU over CoreML on Apple
    // Silicon — and WebGPU is safe with external data, so don't skip.
    #[cfg(all(target_os = "macos", feature = "webgpu"))]
    {
        return false;
    }
    #[cfg(not(all(target_os = "macos", feature = "webgpu")))]
    {
        // Auto would pick CoreML on Apple Silicon (legacy / no-webgpu build)
        // OR fall through to CPU on other platforms. Keep the skip on
        // CoreML-only Apple builds; harmless no-op elsewhere.
        true
    }
}

fn build_exec_config(
    choice: AccelChoice,
    cache_dir: Option<&Path>,
) -> Option<parakeet_rs::ExecutionConfig> {
    info!("parakeet accelerator choice: {:?}", choice);
    match choice {
        AccelChoice::Cpu => {
            _ = cache_dir;
            None
        }
        AccelChoice::Auto => auto_exec_config(cache_dir),
        #[cfg(feature = "coreml")]
        AccelChoice::CoreMl => build_coreml_config(cache_dir),
        #[cfg(feature = "webgpu")]
        AccelChoice::WebGpu => build_webgpu_config(),
    }
}

/// Apple Silicon defaults to WebGPU (Dawn → Metal). parakeet-rs's source
/// notes that CoreML EP currently runs slower than CPU on Parakeet because
/// the ONNX graphs have dynamic input shapes (CoreML claims the nodes but
/// runs them on CPU with overhead), and parakeet-rs hardcodes
/// `ComputeUnits::CPUAndGPU` rather than `CPUAndNeuralEngine` — so CoreML
/// is not the path to ANE acceleration today. WebGPU bypasses that issue
/// at the cost of going through Metal instead of Apple's CoreML compiler.
///
/// Non-macOS targets (Windows/Linux) stay CPU-by-default under Auto — even
/// when the `webgpu` feature is compiled in. Per ADR 0004's validation gate,
/// WebGPU stays behind the explicit `YAPSTACK_PARAKEET_ACCEL=webgpu` opt-in
/// (which routes through `AccelChoice::WebGpu` → `build_webgpu_config`) until
/// Gate C: a canary machine validates it's correct AND faster than CPU on a
/// real Windows GPU. Once Gate C passes, flip this non-macOS Auto arm to
/// `build_webgpu_config()`. Do not flip it before then.
//
// `needless_return` is allowed here because the body is a set of
// mutually-exclusive `#[cfg]` blocks — exactly one is compiled per target ×
// feature combo. The early `return`s let each arm read as a self-contained
// branch; rewriting them as a single tail expression would force an awkward
// cfg-nested if/else that clippy can't see through anyway. Lint-only: no
// runtime behavior changes.
#[allow(clippy::needless_return)]
fn auto_exec_config(cache_dir: Option<&Path>) -> Option<parakeet_rs::ExecutionConfig> {
    #[cfg(all(target_os = "macos", feature = "webgpu"))]
    {
        _ = cache_dir;
        return build_webgpu_config();
    }
    #[cfg(all(target_os = "macos", not(feature = "webgpu"), feature = "coreml"))]
    {
        return build_coreml_config(cache_dir);
    }
    // Auto stays CPU on non-macOS until Gate C (canary validates WebGPU
    // correct + faster than CPU on real Windows GPU); then flip Auto ->
    // WebGPU here. Current opt-in for canary A/B: YAPSTACK_PARAKEET_ACCEL=webgpu.
    #[cfg(not(target_os = "macos"))]
    {
        _ = cache_dir;
        None
    }
    #[cfg(all(target_os = "macos", not(feature = "webgpu"), not(feature = "coreml")))]
    {
        _ = cache_dir;
        None
    }
}

#[cfg(feature = "coreml")]
fn build_coreml_config(cache_dir: Option<&Path>) -> Option<parakeet_rs::ExecutionConfig> {
    use parakeet_rs::{ExecutionConfig, ExecutionProvider};
    let mut cfg = ExecutionConfig::default().with_execution_provider(ExecutionProvider::CoreML);
    if let Some(dir) = cache_dir {
        if let Err(e) = std::fs::create_dir_all(dir) {
            warn!(
                "failed to create CoreML cache dir {} ({e}); proceeding without cache",
                dir.display()
            );
        } else {
            info!("CoreML cache dir: {}", dir.display());
            cfg = cfg.with_coreml_cache_dir(dir);
        }
    } else {
        info!("CoreML enabled, no cache dir provided — recompile cost on every spawn");
    }
    Some(cfg)
}

#[cfg(feature = "webgpu")]
fn build_webgpu_config() -> Option<parakeet_rs::ExecutionConfig> {
    use parakeet_rs::{ExecutionConfig, ExecutionProvider};
    // Dawn drives the WebGPU EP: Metal on macOS, Vulkan/D3D12 on Windows/Linux.
    info!("WebGPU EP requested (Dawn → Metal on macOS, Vulkan/D3D12 elsewhere)");
    Some(ExecutionConfig::default().with_execution_provider(ExecutionProvider::WebGPU))
}

fn assign_speakers(segments: &mut [TranscriptSegment], speakers: &[SpeakerSegment]) {
    for seg in segments {
        let mut best: Option<(u64, u8)> = None;
        for sp in speakers {
            let sp_start_ms = sp.start * 1000 / SORTFORMER_SAMPLE_RATE as u64;
            let sp_end_ms = sp.end * 1000 / SORTFORMER_SAMPLE_RATE as u64;
            let overlap_start = seg.start_ms.max(sp_start_ms);
            let overlap_end = seg.end_ms.min(sp_end_ms);
            if overlap_end > overlap_start {
                let overlap = overlap_end - overlap_start;
                if best.is_none_or(|(b, _)| overlap > b) {
                    best = Some((overlap, sp.speaker_id as u8));
                }
            }
        }
        if let Some((_, id)) = best {
            seg.speaker_id = Some(id);
        }
    }
}

struct GroupedSegment {
    text: String,
    start_secs: f32,
    end_secs: f32,
}

fn group_tokens_into_segments(tokens: &[TimedToken]) -> Vec<GroupedSegment> {
    let mut out: Vec<GroupedSegment> = Vec::new();
    let mut current: Option<GroupedSegment> = None;

    for tok in tokens {
        if tok.text.is_empty() {
            continue;
        }
        match current.as_mut() {
            None => {
                current = Some(GroupedSegment {
                    text: tok.text.trim().to_string(),
                    start_secs: tok.start,
                    end_secs: tok.end,
                });
            }
            Some(seg) => {
                let gap = tok.start - seg.end_secs;
                let span = tok.end - seg.start_secs;
                let break_for_silence = gap > SEGMENT_GAP_SECS;
                let break_for_length = span > MAX_SEGMENT_SECS;
                if break_for_silence || break_for_length {
                    out.push(current.take().unwrap());
                    current = Some(GroupedSegment {
                        text: tok.text.trim().to_string(),
                        start_secs: tok.start,
                        end_secs: tok.end,
                    });
                } else {
                    let needs_space = !seg.text.is_empty()
                        && !tok
                            .text
                            .trim_start()
                            .starts_with(['.', ',', '!', '?', ';', ':', ')']);
                    if needs_space {
                        seg.text.push(' ');
                    }
                    seg.text.push_str(tok.text.trim());
                    seg.end_secs = tok.end;
                }
            }
        }
    }
    if let Some(seg) = current {
        out.push(seg);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(text: &str, start: f32, end: f32) -> TimedToken {
        TimedToken {
            text: text.to_string(),
            start,
            end,
        }
    }

    #[test]
    fn groups_single_phrase_without_gap() {
        let segs = group_tokens_into_segments(&[tok("Hello", 0.0, 0.4), tok("world", 0.5, 0.9)]);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "Hello world");
        assert_eq!(segs[0].start_secs, 0.0);
        assert_eq!(segs[0].end_secs, 0.9);
    }

    #[test]
    fn breaks_segment_on_long_silence() {
        let segs = group_tokens_into_segments(&[
            tok("First", 0.0, 0.3),
            tok("phrase", 0.4, 0.7),
            // 1.5s silence gap — well above SEGMENT_GAP_SECS
            tok("Second", 2.2, 2.5),
            tok("phrase", 2.6, 2.9),
        ]);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "First phrase");
        assert_eq!(segs[1].text, "Second phrase");
    }

    #[test]
    fn breaks_segment_at_length_cap() {
        // Synthesize a long monologue: one token per second for 14s with no gaps.
        let toks: Vec<TimedToken> = (0..14)
            .map(|i| tok("word", i as f32, (i + 1) as f32 - 0.05))
            .collect();
        let segs = group_tokens_into_segments(&toks);
        assert!(segs.len() >= 2, "expected length cap to force a break");
    }

    #[test]
    fn punctuation_does_not_get_extra_space() {
        let segs = group_tokens_into_segments(&[
            tok("Hello", 0.0, 0.3),
            tok(",", 0.31, 0.32),
            tok("world", 0.4, 0.7),
            tok(".", 0.71, 0.72),
        ]);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "Hello, world.");
    }

    #[test]
    fn empty_token_input_yields_no_segments() {
        let segs = group_tokens_into_segments(&[]);
        assert!(segs.is_empty());
    }

    fn ts(start_ms: u64, end_ms: u64) -> TranscriptSegment {
        TranscriptSegment {
            start_ms,
            end_ms,
            text: "x".to_string(),
            confidence: 1.0,
            speaker_id: None,
        }
    }

    fn sp(start_secs: f32, end_secs: f32, id: usize) -> SpeakerSegment {
        SpeakerSegment {
            start: (start_secs * SORTFORMER_SAMPLE_RATE as f32) as u64,
            end: (end_secs * SORTFORMER_SAMPLE_RATE as f32) as u64,
            speaker_id: id,
        }
    }

    #[test]
    fn assigns_speaker_with_full_overlap() {
        let mut segs = vec![ts(0, 1000), ts(2000, 3000)];
        let speakers = vec![sp(0.0, 1.5, 0), sp(1.6, 3.2, 1)];
        assign_speakers(&mut segs, &speakers);
        assert_eq!(segs[0].speaker_id, Some(0));
        assert_eq!(segs[1].speaker_id, Some(1));
    }

    #[test]
    fn assigns_speaker_with_max_overlap_when_segments_straddle() {
        // Transcript segment 0..2000ms straddles two speakers; speaker 1 owns more of it.
        let mut segs = vec![ts(0, 2000)];
        let speakers = vec![sp(0.0, 0.5, 0), sp(0.5, 2.0, 1)];
        assign_speakers(&mut segs, &speakers);
        assert_eq!(segs[0].speaker_id, Some(1));
    }

    #[test]
    fn leaves_speaker_none_when_no_overlap() {
        let mut segs = vec![ts(5000, 6000)];
        let speakers = vec![sp(0.0, 1.0, 0)];
        assign_speakers(&mut segs, &speakers);
        assert_eq!(segs[0].speaker_id, None);
    }

    #[test]
    fn assign_speakers_with_empty_diarization_is_noop() {
        let mut segs = vec![ts(0, 1000), ts(1000, 2000)];
        assign_speakers(&mut segs, &[]);
        assert!(segs.iter().all(|s| s.speaker_id.is_none()));
    }
}
