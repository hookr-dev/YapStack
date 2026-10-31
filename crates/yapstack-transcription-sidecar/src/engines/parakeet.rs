//! Parakeet TDT-v3 backend (parakeet-rs 0.3 + ONNX Runtime via `ort`).

use std::path::{Path, PathBuf};

use parakeet_rs::sortformer::{Sortformer, SpeakerSegment};
use parakeet_rs::{ParakeetTDT, TimedToken, TimestampMode, Transcriber};
use tracing::{debug, info, warn};
use yapstack_common::types::TranscriptSegment;

use crate::engines::ort_placement::{self, PlacementRecorder, SessionPlacement};
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

impl ParakeetBackend {
    /// Loads the model under an explicit accelerator choice. With a
    /// `placement` recorder attached, returns ORT's node placement for each
    /// session of the model that ended up loaded (encoder first).
    fn load_model_with(
        &mut self,
        model_path: &Path,
        choice: AccelChoice,
        placement: Option<PlacementRecorder>,
    ) -> Result<Vec<SessionPlacement>, String> {
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
        let target = resolve_accel(choice, ModelDirTraits::inspect(model_path));
        info!(
            marker = "live_accel_choice",
            engine = "parakeet",
            requested = ?choice,
            attempted = target.label(),
            model_dir = %model_path.display(),
            "parakeet acceleration resolved"
        );
        let attempt = ParakeetTDT::from_pretrained(
            model_path,
            build_exec_config(target, cache_dir, placement.as_ref()),
        );
        // `loaded` diverges from `target` only on the fallback path below.
        let (mut model, loaded, accel_fallback) = match attempt {
            Ok(m) => (m, target, None),
            Err(e) if target != AccelTarget::Cpu => {
                warn!(
                    marker = "live_accel_fallback",
                    engine = "parakeet",
                    requested = target.label(),
                    error = %e,
                    "accelerator load failed; falling back to CPU \
                     (set YAPSTACK_PARAKEET_ACCEL=cpu to suppress this attempt)"
                );
                // Drop whatever the failed attempt reported so the
                // placement below describes only the sessions that loaded.
                if let Some(recorder) = placement.as_ref() {
                    recorder.take();
                }
                let m = ParakeetTDT::from_pretrained(
                    model_path,
                    build_exec_config(AccelTarget::Cpu, cache_dir, placement.as_ref()),
                )
                .map_err(|e2| format!("failed to load parakeet model (CPU fallback): {e2}"))?;
                let reason = format!("{} failed to load: {e}", target.label());
                (m, AccelTarget::Cpu, Some(reason))
            }
            Err(e) => return Err(format!("failed to load parakeet model: {e}")),
        };

        let sessions = placement.map(|r| r.take()).unwrap_or_default();
        log_placement(loaded, &sessions);

        warm_up(&mut model);
        self.model = Some(model);
        self.last_engine_info = Some(EngineInfo {
            accel: loaded.label().to_string(),
            model_dir: model_path.display().to_string(),
            accel_fallback,
        });
        info!(
            "parakeet TDT model loaded in {} ms",
            load_start.elapsed().as_millis()
        );
        Ok(sessions)
    }
}

impl TranscriptionBackend for ParakeetBackend {
    fn load_model(&mut self, model_path: &Path) -> Result<(), String> {
        self.load_model_with(
            model_path,
            AccelChoice::from_env(),
            PlacementRecorder::from_env(),
        )
        .map(drop)
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

/// Execution provider a load attempts, after `AccelChoice` is resolved
/// against the build and the model directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccelTarget {
    Cpu,
    #[cfg(feature = "coreml")]
    CoreMl,
    #[cfg(feature = "webgpu")]
    WebGpu,
}

impl AccelTarget {
    /// Stable label for the `ModelLoaded.accel` telemetry payload — kept
    /// lowercase + ASCII for grep-friendliness.
    fn label(self) -> &'static str {
        match self {
            AccelTarget::Cpu => "cpu",
            #[cfg(feature = "coreml")]
            AccelTarget::CoreMl => "coreml",
            #[cfg(feature = "webgpu")]
            AccelTarget::WebGpu => "webgpu",
        }
    }
}

/// What the accelerator decision needs to know about a model directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModelDirTraits {
    /// An external `.onnx.data` initializer is present.
    has_external_data: bool,
    /// The encoder graph parakeet-rs will load passes [`gpu_placement_ok`].
    gpu_placement_ok: bool,
}

impl ModelDirTraits {
    fn inspect(model_dir: &Path) -> Self {
        Self {
            has_external_data: has_external_data_file(model_dir),
            gpu_placement_ok: find_encoder_file_name(model_dir)
                .is_some_and(|name| gpu_placement_ok(&name)),
        }
    }
}

/// Whether the WebGPU EP places this encoder graph's heavy (MatMul/Conv
/// family) ops on the GPU — gate G4 is >= 90%. Placement is a property of
/// graph × ORT build × EP, not of the user's hardware, so a table measured
/// with `YAPSTACK_ORT_PLACEMENT=1` (ORT 1.24, the `placement` ignored test
/// below) is the honest source. Re-measure when the graph or ORT changes.
/// Unlisted graphs are never attempted on the GPU under Auto.
fn gpu_placement_ok(encoder_file_name: &str) -> bool {
    match encoder_file_name {
        // fp32 export: MatMul 289/289 and Conv 77/77 on WebGPU (100%).
        "encoder-model.onnx" => true,
        // ORT *dynamic* int8: the WebGPU EP has no MatMulInteger,
        // ConvInteger or DynamicQuantizeLinear kernels, so 294 of the 366
        // heavy nodes run on CPU (19.7% on GPU) and every layer copies
        // tensors across the boundary — half the speed of plain CPU.
        "encoder-model.int8.onnx" => false,
        _ => false,
    }
}

/// The encoder file parakeet-rs 0.3.5 picks from a model directory: the
/// first of its fixed candidates that exists, else the first `encoder*.onnx`
/// (`model_tdt.rs` `find_encoder`). Mirrored so the accelerator decision is
/// about the graph that actually loads.
fn find_encoder_file_name(model_dir: &Path) -> Option<String> {
    const CANDIDATES: [&str; 3] = [
        "encoder-model.onnx",
        "encoder.onnx",
        "encoder-model.int8.onnx",
    ];
    if let Some(name) = CANDIDATES.iter().find(|c| model_dir.join(c).exists()) {
        return Some((*name).to_string());
    }
    std::fs::read_dir(model_dir)
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .find(|name| name.starts_with("encoder") && name.ends_with(".onnx"))
}

/// Resolve the user's (or Auto's) choice for this model directory.
/// Explicit choices are honored regardless of the model: a hand-set
/// `YAPSTACK_PARAKEET_ACCEL=webgpu` still attempts WebGPU on int8, so the
/// two can be compared.
fn resolve_accel(choice: AccelChoice, model: ModelDirTraits) -> AccelTarget {
    match choice {
        AccelChoice::Auto => resolve_auto(model),
        AccelChoice::Cpu => AccelTarget::Cpu,
        #[cfg(feature = "coreml")]
        AccelChoice::CoreMl => AccelTarget::CoreMl,
        #[cfg(feature = "webgpu")]
        AccelChoice::WebGpu => AccelTarget::WebGpu,
    }
}

/// Apple Silicon Auto attempts WebGPU (Dawn → Metal), but only for encoder
/// graphs that pass [`gpu_placement_ok`]; anything else runs on CPU, which
/// is both honest and faster than a GPU session doing its heavy ops on
/// CPU. CoreML is not the Auto path: parakeet-rs hardcodes
/// `ComputeUnits::CPUAndGPU` and the dynamic input shapes leave the heavy
/// ops on CPU anyway. The CoreML-only fallback build keeps skipping models
/// with external `.onnx.data` (ORT's `model_path must not be empty` bug on
/// CoreML partitioning; WebGPU doesn't serialize the partitioned subgraph,
/// so it is safe with external data).
///
/// Non-macOS targets (Windows/Linux) attempt WebGPU under Auto since Gate C
/// passed (2026-07-05: the Windows canary validated WebGPU correct AND
/// clearly faster than CPU per ADR 0004's validation gate).
///
/// Every WebGPU registration is strict, so an EP-init failure surfaces
/// through `load_model_with`'s explicit, logged CPU fallback (honest
/// label, `accel_fallback` reason) rather than ort's silent one.
/// `YAPSTACK_PARAKEET_ACCEL=cpu` remains the escape hatch.
//
// `needless_return` is allowed because the body is a set of mutually
// exclusive `#[cfg]` blocks — exactly one is compiled per target × feature
// combo — and the early `return`s let each read as a self-contained arm.
#[allow(clippy::needless_return)]
fn resolve_auto(model: ModelDirTraits) -> AccelTarget {
    #[cfg(all(target_os = "macos", feature = "webgpu"))]
    {
        _ = model.has_external_data;
        return if model.gpu_placement_ok {
            AccelTarget::WebGpu
        } else {
            AccelTarget::Cpu
        };
    }
    #[cfg(all(target_os = "macos", not(feature = "webgpu"), feature = "coreml"))]
    {
        _ = model.gpu_placement_ok;
        return if model.has_external_data {
            AccelTarget::Cpu
        } else {
            AccelTarget::CoreMl
        };
    }
    // Gate C measured int8 + WebGPU faster than CPU on the Windows canary,
    // so the placement gate is not applied here; a non-macOS placement
    // measurement is what would change that.
    #[cfg(all(not(target_os = "macos"), feature = "webgpu"))]
    {
        _ = (model.has_external_data, model.gpu_placement_ok);
        return AccelTarget::WebGpu;
    }
    #[cfg(not(any(
        all(target_os = "macos", any(feature = "webgpu", feature = "coreml")),
        all(not(target_os = "macos"), feature = "webgpu")
    )))]
    {
        _ = (model.has_external_data, model.gpu_placement_ok);
        AccelTarget::Cpu
    }
}

/// The parakeet-rs config for a target. `None` is parakeet-rs's default
/// (CPU EP). A placement recorder, when given, is attached to every session
/// on top of the target's own configuration.
fn build_exec_config(
    target: AccelTarget,
    cache_dir: Option<&Path>,
    placement: Option<&PlacementRecorder>,
) -> Option<parakeet_rs::ExecutionConfig> {
    info!("parakeet accelerator target: {:?}", target);
    let config = match target {
        AccelTarget::Cpu => {
            _ = cache_dir;
            None
        }
        #[cfg(feature = "coreml")]
        AccelTarget::CoreMl => Some(build_coreml_config(cache_dir)),
        #[cfg(feature = "webgpu")]
        AccelTarget::WebGpu => Some(build_webgpu_config_strict()),
    };
    match placement {
        None => config,
        Some(recorder) => Some(with_placement_recorder(
            config.unwrap_or_default(),
            recorder.clone(),
        )),
    }
}

/// Chains the recorder after any `configure` hook the config already has
/// (parakeet-rs keeps a single hook, so setting a second would replace it).
fn with_placement_recorder(
    config: parakeet_rs::ExecutionConfig,
    recorder: PlacementRecorder,
) -> parakeet_rs::ExecutionConfig {
    let inner = config.configure.clone();
    config.with_custom_configure(move |builder| {
        let builder = match inner.as_ref() {
            Some(configure) => configure(builder)?,
            None => builder,
        };
        recorder.attach(builder)
    })
}

/// One `ort_placement` line per loaded session (encoder, then
/// decoder_joint), for the opt-in diagnostics.
fn log_placement(loaded: AccelTarget, sessions: &[SessionPlacement]) {
    const SESSION_NAMES: [&str; 2] = ["encoder", "decoder_joint"];
    let ep = ort_placement::ort_ep_name(loaded.label());
    for (i, s) in sessions.iter().enumerate() {
        info!(
            marker = "ort_placement",
            session = SESSION_NAMES.get(i).copied().unwrap_or("other"),
            ep = loaded.label(),
            heavy_on_ep = %s.heavy_on_label(ep),
            cpu_nodes = s.nodes_by_ep.get("CPUExecutionProvider").copied().unwrap_or(0),
            nodes_by_ep = ?s.nodes_by_ep,
            heavy_by_ep = ?s.heavy_by_ep,
            "ort node placement"
        );
    }
}

/// One silent second through the full pipeline right after load, so the
/// first real chunk doesn't pay the cold-start cost (allocator arenas,
/// WebGPU pipeline compilation): measured cold runs on WebGPU were up to
/// ~45% slower than steady state. A failure is only logged — a broken model surfaces on the
/// first real transcribe with its own error.
fn warm_up(model: &mut ParakeetTDT) {
    let start = std::time::Instant::now();
    let silence = vec![0.0_f32; PARAKEET_SAMPLE_RATE as usize];
    match model.transcribe_samples(silence, PARAKEET_SAMPLE_RATE, 1, Some(TimestampMode::Words)) {
        Ok(_) => info!(
            "parakeet warmup inference in {} ms",
            start.elapsed().as_millis()
        ),
        Err(e) => warn!("parakeet warmup inference failed: {e}"),
    }
}

#[cfg(feature = "coreml")]
fn build_coreml_config(cache_dir: Option<&Path>) -> parakeet_rs::ExecutionConfig {
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
    cfg
}

/// Strict WebGPU registration (Dawn → Metal on macOS, Vulkan/D3D12
/// elsewhere): the EP carries `error_on_failure`, so an unavailable
/// adapter/driver fails session creation loudly instead of silently
/// degrading to CPU under a `webgpu` label. parakeet-rs's own WebGPU arm
/// registers the EP soft, so we pass `Cpu` (a no-op arm) and register
/// providers ourselves via the `configure` hook, which parakeet-rs applies
/// after its own arm. CPU stays registered AFTER WebGPU as the per-node
/// fallback for ops the EP doesn't implement — node placement within a
/// working GPU session, not session-level fallback. Memory patterns are
/// disabled per WebGPU-EP guidance for dynamic input shapes (the TDT
/// transducer reshapes every chunk).
#[cfg(feature = "webgpu")]
fn build_webgpu_config_strict() -> parakeet_rs::ExecutionConfig {
    use parakeet_rs::{ExecutionConfig, ExecutionProvider};
    info!("WebGPU EP requested (strict: registration failure is a hard error)");
    ExecutionConfig::default()
        .with_execution_provider(ExecutionProvider::Cpu)
        .with_custom_configure(|builder| {
            Ok(builder
                .with_memory_pattern(false)?
                .with_execution_providers([
                    ort::ep::WebGPU::default().build().error_on_failure(),
                    ort::ep::CPU::default().build().error_on_failure(),
                ])?)
        })
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

    // --- accelerator resolution ---

    /// A throwaway model dir holding empty files with the given names.
    fn model_dir(tag: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "yapstack-parakeet-accel-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in files {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        dir
    }

    const INT8_FILES: [&str; 4] = [
        "encoder-model.int8.onnx",
        "decoder_joint-model.int8.onnx",
        "nemo128.onnx",
        "vocab.txt",
    ];
    const FP32_FILES: [&str; 4] = [
        "encoder-model.onnx",
        "encoder-model.onnx.data",
        "decoder_joint-model.onnx",
        "vocab.txt",
    ];

    #[test]
    fn gpu_placement_table_rejects_dynamic_int8_and_unknown_graphs() {
        assert!(gpu_placement_ok("encoder-model.onnx"));
        assert!(!gpu_placement_ok("encoder-model.int8.onnx"));
        assert!(!gpu_placement_ok("encoder-model.somethingelse.onnx"));
    }

    #[test]
    fn finds_the_encoder_parakeet_rs_would_load() {
        let int8 = model_dir("find-int8", &INT8_FILES);
        assert_eq!(
            find_encoder_file_name(&int8).as_deref(),
            Some("encoder-model.int8.onnx")
        );
        // The fixed candidates win over the fallback scan, in order.
        let both = model_dir(
            "find-both",
            &["encoder-model.int8.onnx", "encoder-model.onnx"],
        );
        assert_eq!(
            find_encoder_file_name(&both).as_deref(),
            Some("encoder-model.onnx")
        );
        let other = model_dir("find-other", &["encoder-model.fp16.onnx"]);
        assert_eq!(
            find_encoder_file_name(&other).as_deref(),
            Some("encoder-model.fp16.onnx")
        );
        assert_eq!(find_encoder_file_name(&model_dir("find-none", &[])), None);
    }

    #[test]
    fn inspects_model_dir_traits() {
        assert_eq!(
            ModelDirTraits::inspect(&model_dir("traits-int8", &INT8_FILES)),
            ModelDirTraits {
                has_external_data: false,
                gpu_placement_ok: false,
            }
        );
        assert_eq!(
            ModelDirTraits::inspect(&model_dir("traits-fp32", &FP32_FILES)),
            ModelDirTraits {
                has_external_data: true,
                gpu_placement_ok: true,
            }
        );
    }

    const INT8: ModelDirTraits = ModelDirTraits {
        has_external_data: false,
        gpu_placement_ok: false,
    };
    const FP32: ModelDirTraits = ModelDirTraits {
        has_external_data: true,
        gpu_placement_ok: true,
    };

    #[test]
    fn explicit_cpu_is_cpu_for_every_model() {
        for model in [INT8, FP32] {
            assert_eq!(resolve_accel(AccelChoice::Cpu, model).label(), "cpu");
        }
    }

    /// The shipped int8 runs its quantized ops on CPU under WebGPU, at half
    /// plain-CPU speed — so macOS Auto resolves it to CPU and says so.
    #[cfg(all(target_os = "macos", feature = "webgpu"))]
    #[test]
    fn macos_auto_resolves_int8_to_cpu_and_fp32_to_webgpu() {
        assert_eq!(resolve_accel(AccelChoice::Auto, INT8), AccelTarget::Cpu);
        assert_eq!(resolve_accel(AccelChoice::Auto, INT8).label(), "cpu");
        assert_eq!(resolve_accel(AccelChoice::Auto, FP32).label(), "webgpu");
    }

    #[cfg(all(not(target_os = "macos"), feature = "webgpu"))]
    #[test]
    fn non_macos_auto_keeps_attempting_webgpu() {
        assert_eq!(resolve_accel(AccelChoice::Auto, INT8).label(), "webgpu");
    }

    #[cfg(not(feature = "webgpu"))]
    #[test]
    fn auto_without_webgpu_never_claims_webgpu() {
        for model in [INT8, FP32] {
            assert_ne!(resolve_accel(AccelChoice::Auto, model).label(), "webgpu");
        }
    }

    /// An explicit opt-in is honored even for int8, so GPU-vs-CPU can still
    /// be compared on purpose.
    #[cfg(feature = "webgpu")]
    #[test]
    fn explicit_webgpu_is_honored_for_int8() {
        assert_eq!(resolve_accel(AccelChoice::WebGpu, INT8).label(), "webgpu");
    }

    /// WebGPU must never take parakeet-rs's soft arm (silent CPU fallback
    /// under a webgpu label): the config passes the no-op `Cpu` arm and
    /// registers WebGPU strictly through the configure hook.
    #[cfg(feature = "webgpu")]
    #[test]
    fn webgpu_config_registers_the_ep_strictly() {
        let cfg = build_exec_config(AccelTarget::WebGpu, None, None).expect("config");
        assert_eq!(cfg.execution_provider, parakeet_rs::ExecutionProvider::Cpu);
        assert!(cfg.configure.is_some());
    }

    #[test]
    fn cpu_config_is_parakeet_default_unless_recording_placement() {
        assert!(build_exec_config(AccelTarget::Cpu, None, None).is_none());
        let recorder = PlacementRecorder::default();
        let cfg = build_exec_config(AccelTarget::Cpu, None, Some(&recorder)).expect("config");
        assert_eq!(cfg.execution_provider, parakeet_rs::ExecutionProvider::Cpu);
        assert!(cfg.configure.is_some());
    }

    /// Gate G4 on a real model: the EP Auto reports must hold >= 90% of the
    /// encoder's MatMul/Conv-family nodes. Red for int8 under the old
    /// "always WebGPU" Auto (19.7%); green for int8 → CPU and fp32 → WebGPU.
    #[test]
    #[ignore = "needs YAPSTACK_TEST_PARAKEET_DIR set to a downloaded Parakeet model dir"]
    fn auto_places_heavy_ops_on_the_reported_ep() {
        let dir =
            std::env::var("YAPSTACK_TEST_PARAKEET_DIR").expect("set YAPSTACK_TEST_PARAKEET_DIR");
        let mut backend = ParakeetBackend::new(None, None);
        let sessions = backend
            .load_model_with(
                Path::new(&dir),
                AccelChoice::Auto,
                Some(PlacementRecorder::default()),
            )
            .expect("model loads");
        let reported = backend.engine_info().expect("engine info").accel;
        let ep = ort_placement::ort_ep_name(&reported);
        let encoder = sessions.first().expect("encoder placement recorded");
        let share = encoder.heavy_share(ep);
        assert!(
            share >= 0.90,
            "reported {reported} but only {:.1}% of MatMul/Conv nodes placed there: {:?}",
            100.0 * share,
            encoder.heavy_by_ep
        );
    }
}
