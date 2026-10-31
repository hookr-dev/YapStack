use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{Emitter, Manager};
use tokio::sync::Mutex;
use tracing::{error, info};
use yapstack_common::engines::engine_catalogue;
use yapstack_common::types::EngineKind;
use yapstack_transcription::{
    ModelManager, ModelSize, ParakeetVariant, SortformerVariant, TranscriptionClient,
};

use super::error::CommandError;

// --- DTOs ---

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub enum ModelSizeDto {
    Tiny,
    Base,
    Small,
    Medium,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct ModelInfoDto {
    pub size: ModelSizeDto,
    pub downloaded: bool,
    pub path: Option<String>,
    pub display_name: String,
    pub approximate_size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct TranscriptSegmentDto {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub confidence: f32,
    /// Populated when the active engine is Parakeet *and* diarization
    /// was requested for the originating transcribe call. `None` for Whisper.
    pub speaker_id: Option<u8>,
}

/// Which transcription engine the frontend has selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum EngineKindDto {
    Whisper,
    Parakeet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum ParakeetVariantDto {
    TdtV3,
    /// int8-quantized variant — same model, smaller footprint, no
    /// external `.onnx.data` so accelerators can load it. Used on
    /// Apple Silicon by default; the FE migration coerces existing
    /// `TdtV3` users on Apple Silicon to this variant on next launch.
    TdtV3Int8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum SortformerVariantDto {
    V2_1,
}

/// Engine capabilities + supported languages for the cascading UI in Settings.
#[derive(Debug, Clone, Serialize, Type)]
pub struct EngineDescriptorDto {
    pub kind: EngineKindDto,
    pub display_name: String,
    pub languages: Vec<String>,
    pub supports_diarization: bool,
    pub supports_initial_prompt: bool,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct ParakeetModelInfoDto {
    pub variant: ParakeetVariantDto,
    pub downloaded: bool,
    pub display_name: String,
    pub approximate_size_bytes: u64,
}

/// Emitted as a Tauri event (`transcription-engine-loaded`) after a
/// transcription client successfully initializes — both on initial
/// app launch and on any subsequent engine/variant switch. Lets the
/// frontend show a "Parakeet · WebGPU" badge keyed to ground truth
/// from the sidecar instead of inferring from FE state. The
/// `model_dir` ends in the variant directory (e.g.
/// `…/parakeet-tdt-v3-int8`) so the FE can derive whether int8 or
/// fp32 is active without a separate field.
#[derive(Debug, Clone, Serialize, Type)]
pub struct TranscriptionEngineLoadedEvent {
    pub engine: EngineKindDto,
    pub accel: Option<String>,
    pub model_dir: Option<String>,
    /// Why the attempted accelerator failed to load, when `accel` is a
    /// CPU fallback. `None` when the accelerator loaded or CPU was chosen.
    pub accel_fallback: Option<String>,
    /// `accel` is a GPU EP that leaves part of the model's heavy ops on
    /// the CPU (static placement table in the sidecar).
    pub accel_partial: bool,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct SortformerModelInfoDto {
    pub variant: SortformerVariantDto,
    pub downloaded: bool,
    pub display_name: String,
    pub approximate_size_bytes: u64,
}

// --- From impls ---

impl From<ModelSizeDto> for ModelSize {
    fn from(s: ModelSizeDto) -> Self {
        match s {
            ModelSizeDto::Tiny => ModelSize::Tiny,
            ModelSizeDto::Base => ModelSize::Base,
            ModelSizeDto::Small => ModelSize::Small,
            ModelSizeDto::Medium => ModelSize::Medium,
        }
    }
}

impl From<ModelSize> for ModelSizeDto {
    fn from(s: ModelSize) -> Self {
        match s {
            ModelSize::Tiny => ModelSizeDto::Tiny,
            ModelSize::Base => ModelSizeDto::Base,
            ModelSize::Small => ModelSizeDto::Small,
            ModelSize::Medium => ModelSizeDto::Medium,
        }
    }
}

impl From<EngineKindDto> for EngineKind {
    fn from(k: EngineKindDto) -> Self {
        match k {
            EngineKindDto::Whisper => EngineKind::Whisper,
            EngineKindDto::Parakeet => EngineKind::Parakeet,
        }
    }
}

impl From<EngineKind> for EngineKindDto {
    fn from(k: EngineKind) -> Self {
        match k {
            EngineKind::Whisper => EngineKindDto::Whisper,
            EngineKind::Parakeet => EngineKindDto::Parakeet,
        }
    }
}

impl From<ParakeetVariantDto> for ParakeetVariant {
    fn from(v: ParakeetVariantDto) -> Self {
        match v {
            ParakeetVariantDto::TdtV3 => ParakeetVariant::TdtV3,
            ParakeetVariantDto::TdtV3Int8 => ParakeetVariant::TdtV3Int8,
        }
    }
}

impl From<ParakeetVariant> for ParakeetVariantDto {
    fn from(v: ParakeetVariant) -> Self {
        match v {
            ParakeetVariant::TdtV3 => ParakeetVariantDto::TdtV3,
            ParakeetVariant::TdtV3Int8 => ParakeetVariantDto::TdtV3Int8,
        }
    }
}

impl From<SortformerVariantDto> for SortformerVariant {
    fn from(v: SortformerVariantDto) -> Self {
        match v {
            SortformerVariantDto::V2_1 => SortformerVariant::V2_1,
        }
    }
}

impl From<yapstack_transcription::ModelInfo> for ModelInfoDto {
    fn from(m: yapstack_transcription::ModelInfo) -> Self {
        Self {
            size: m.size.into(),
            downloaded: m.downloaded,
            path: m.path.map(|p| p.to_string_lossy().into_owned()),
            display_name: m.display_name,
            approximate_size_bytes: m.approximate_size_bytes,
        }
    }
}

// --- State types ---

pub type ModelManagerState = Arc<Mutex<ModelManager>>;

/// What engine + variant + diarization config the currently-initialized
/// scheduler was constructed for. `init_transcription_client` is idempotent
/// when called with a matching config and rejects with an explicit
/// "shut down first" error when called with a different one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConfig {
    pub kind: EngineKind,
    pub whisper_model: Option<ModelSize>,
    pub parakeet_variant: Option<ParakeetVariant>,
    pub diarization: bool,
}

/// Both pieces of "the engine is initialized" state, paired so the config
/// is always kept in sync with the live scheduler.
pub struct InitializedEngine {
    pub config: EngineConfig,
    pub scheduler: Arc<super::transcription_scheduler::TranscriptionScheduler>,
}

/// The single source of truth for engine initialization. The scheduler owns
/// the `TranscriptionClient` for its full lifetime; both the session live
/// loop and the dictation live loop clone `Arc<TranscriptionScheduler>` from
/// this state. `None` means engine is not initialized; `Some` means a
/// scheduler is live (running or shutting down — the scheduler's own state
/// machine handles the in-between).
pub type TranscriptionSchedulerState = Arc<Mutex<Option<InitializedEngine>>>;

/// A range of mic ring-buffer write positions owned by a dictation runtime.
/// Both endpoints are in raw-sample units (matches `mic_buffer_info().
/// samples_written`). Samples in `[start, end)` were captured during
/// dictation and must not appear in any session transcript or WAV export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicWindow {
    pub start: u64,
    pub end: u64,
}

impl MicWindow {
    /// Treat the window as a half-open `[start, end)` interval and test
    /// overlap against another `[other_start, other_end)`.
    pub fn overlaps(&self, other_start: u64, other_end: u64) -> bool {
        self.start < other_end && self.end > other_start
    }
}

/// Internal mic-ownership state — open window plus a deque of closed
/// windows that the session loop and WAV writer haven't fully consumed.
/// Held under a single `Mutex` because reads happen at poll cadence
/// (every ~30 ms) and writes happen only at dictation start / end /
/// session start / per-tick prune, all of which are short.
///
/// `closed` is a `VecDeque` so the session loop can `prune_before` the
/// minimum cursor position of all consumers (mic VAD + WAV writer) on
/// each tick — popping front entries whose `end` is already behind the
/// slowest consumer keeps the list bounded by the number of
/// in-flight-or-recently-finished dictations regardless of session
/// length. Without pruning, a long session with many dictations would
/// grow a list that's cloned on every poll.
#[derive(Default)]
pub struct DictationMicState {
    /// Currently-open dictation window (start_pos). `Some` while a
    /// dictation runtime owns the mic. The end position is unknown
    /// until the dictation finalizer runs.
    pub open: Option<u64>,
    /// Closed dictation windows still in scope for at least one
    /// consumer (session mic loop / WAV writer). Pruned from the front
    /// once both consumers have advanced past `window.end`. Always
    /// ordered by `start` because dictations land in chronological
    /// order on `close()`.
    pub closed: std::collections::VecDeque<MicWindow>,
}

/// Mic-ownership coordination between the session and dictation live loops.
/// Replaces the prior boolean+atomic flag with explicit `[start, end)`
/// windows that the session can reason about precisely:
///
/// - Catches dictations that open and close between two session polls
///   (the boolean would have flickered without the session ever observing
///   it; a closed window is durable).
/// - Lets dictation initialize its own VAD/WAV cursors at `start` so audio
///   captured between hotkey press and runtime spawn isn't dropped on
///   the floor.
/// - Lets WAV muting compute precise byte/sample ranges to zero, instead
///   of zeroing whole batches and losing system audio in Mixed mode.
pub struct DictationOwnsMic {
    state: std::sync::Mutex<DictationMicState>,
}

impl DictationOwnsMic {
    pub fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(DictationMicState::default()),
        }
    }

    /// Open a new dictation window. Called by `start_live_transcription`
    /// for `Dictation` runtimes after the slot guard passes. Returns the
    /// recorded `start` so the caller can pass it into the dictation
    /// runtime's cursor init.
    pub fn open(&self, start: u64) -> u64 {
        let mut s = self.state.lock().expect("dictation-owns-mic poisoned");
        s.open = Some(start);
        start
    }

    /// Close the currently-open window with `end`. Called by the dictation
    /// finalizer (and as a safety net by the runtime's RAII guard). If
    /// no window is open, this is a no-op — happens when the runtime
    /// errored out before opening or when the session already cleared
    /// state via `clear_for_session`. Empty windows (`end <= start`) are
    /// dropped rather than recorded.
    pub fn close(&self, end: u64) {
        let mut s = self.state.lock().expect("dictation-owns-mic poisoned");
        if let Some(start) = s.open.take() {
            if end > start {
                s.closed.push_back(MicWindow { start, end });
            }
        }
    }

    /// Reset all state. Called at session start so a fresh session
    /// observes only windows that occur during its lifetime, not stale
    /// entries from a prior session that crashed without draining.
    /// Called only when the dictation slot is genuinely Idle — otherwise
    /// we'd unwind a live dictation's open window.
    pub fn clear_for_session(&self) {
        let mut s = self.state.lock().expect("dictation-owns-mic poisoned");
        s.open = None;
        s.closed.clear();
    }

    /// Clear the currently-open window without recording it as closed.
    /// Used by the dictation runtime's RAII guard on Drop for early-
    /// error paths that opened the window but never spawned the live
    /// loop — no audio was actually transcribed, so there's nothing to
    /// record. The normal close path on the spawned task's finalizer
    /// is unaffected: by the time the guard drops in the slot's
    /// transition to Idle, `open` is already None and this is a no-op.
    pub fn clear_open(&self) {
        let mut s = self.state.lock().expect("dictation-owns-mic poisoned");
        s.open = None;
    }

    /// Pop closed windows whose `end <= min_pos`. `min_pos` is the
    /// minimum mic ring-buffer position across every consumer that
    /// reads this state (session mic VAD cursor, session-WAV
    /// `flush_positions.mic_pos`); a window whose `end` is already
    /// behind every consumer can never affect any future decision and
    /// is safe to drop. Called once per session tick to keep the list
    /// bounded under sessions with many dictations.
    pub fn prune_before(&self, min_pos: u64) {
        let mut s = self.state.lock().expect("dictation-owns-mic poisoned");
        while s.closed.front().is_some_and(|w| w.end <= min_pos) {
            s.closed.pop_front();
        }
    }

    /// Cheap snapshot for a single decision: clones the closed windows
    /// (small Vec) and copies the open-window start. Use the snapshot's
    /// `windows()` to iterate all (open + closed) in one place.
    pub fn snapshot(&self) -> DictationMicSnapshot {
        let s = self.state.lock().expect("dictation-owns-mic poisoned");
        DictationMicSnapshot {
            open: s.open,
            closed: s.closed.clone(),
        }
    }

    /// Atomically rebind mic-ownership state across a mic ring-buffer
    /// replacement. All positions stored here live in the mic ring's
    /// coordinate space; once that ring is swapped out (device-format
    /// change), every previously-recorded `start`/`end` value is a
    /// reference into a buffer the session can no longer read.
    ///
    /// Semantics:
    /// - **All closed windows are drained** and returned to the caller.
    ///   They cannot be consumed against the new buffer (their `end`
    ///   could be well past the new buffer's `write_pos`), so leaving
    ///   them in place would corrupt the next session tick's overlap
    ///   math. The caller uses the returned windows to decide whether
    ///   pre-swap rescue ranges need to be truncated (a dictated range
    ///   inside the rescue range must not leak into session content).
    /// - **The open window's `start` is rebased** to `new_open_start`
    ///   (typically the new buffer's `write_pos` at the swap instant).
    ///   The dictation runtime experiences the same swap on its own
    ///   `SourceVadState` and will record the eventual `close(end)` in
    ///   new-buffer coordinates; rebasing the start keeps both sides
    ///   of the window in the same coordinate system. If no window was
    ///   open, this is a no-op.
    /// - Returns the prior open start (if any) so the caller can
    ///   truncate any rescue range that overlaps the original open
    ///   start in old coordinates.
    ///
    /// Unlike `clear_for_session`, this is safe to call while a
    /// dictation is actively owning the mic — the runtime's
    /// in-progress transcription continues uninterrupted, just in
    /// the new buffer's coordinate space.
    pub fn rebase_for_mic_buffer_replacement(&self, new_open_start: u64) -> DictationRebaseResult {
        let mut s = self.state.lock().expect("dictation-owns-mic poisoned");
        let dropped_closed: std::collections::VecDeque<MicWindow> = std::mem::take(&mut s.closed);
        let prior_open = s.open;
        if prior_open.is_some() {
            s.open = Some(new_open_start);
        }
        DictationRebaseResult {
            dropped_closed,
            prior_open,
            new_open: s.open,
        }
    }
}

/// Result of `DictationOwnsMic::rebase_for_mic_buffer_replacement`. The
/// caller uses `dropped_closed` + `prior_open` to decide whether a
/// pre-swap rescue range overlaps any dictation window — if so, the
/// rescue must be truncated at the earliest dictation boundary to keep
/// dictated audio out of the session transcript / WAV.
#[derive(Debug)]
pub struct DictationRebaseResult {
    /// Closed windows that were dropped from the queue. Their
    /// `start`/`end` are in the OLD mic ring's coordinate space; only
    /// useful for boundary computations against pre-swap rescue ranges
    /// (also in OLD coordinates).
    pub dropped_closed: std::collections::VecDeque<MicWindow>,
    /// The open window's start in OLD coordinates, before the rebase.
    /// `None` if no window was open at the moment of swap.
    pub prior_open: Option<u64>,
    /// The open window's start in NEW coordinates, after the rebase.
    /// `Some(new_open_start)` if a window was open; `None` otherwise.
    /// Mirrors what subsequent `snapshot()` calls will return.
    pub new_open: Option<u64>,
}

impl Default for DictationOwnsMic {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot of mic-ownership state for one decision. The open window
/// (if any) is treated as `[start, u64::MAX)` for overlap math — every
/// future sample is dictation-owned until the close lands.
pub struct DictationMicSnapshot {
    pub open: Option<u64>,
    pub closed: std::collections::VecDeque<MicWindow>,
}

impl DictationMicSnapshot {
    /// Returns true if the buffer-position range `[start, end)` overlaps
    /// any dictation window, open or closed.
    pub fn overlaps(&self, start: u64, end: u64) -> bool {
        if let Some(open) = self.open {
            if open < end {
                return true;
            }
        }
        self.closed.iter().any(|w| w.overlaps(start, end))
    }

    /// Iterate all windows (closed in chronological order, then the
    /// virtual open window). `closed` is already ordered by `start`
    /// because dictations land sequentially via `close()`, so no sort
    /// is needed; the open window's `start` is by construction past
    /// every closed window's `end`.
    pub fn iter_all(&self) -> impl Iterator<Item = MicWindow> + '_ {
        self.closed
            .iter()
            .copied()
            .chain(self.open.map(|start| MicWindow {
                start,
                end: u64::MAX,
            }))
    }
}

pub type DictationOwnsMicState = Arc<DictationOwnsMic>;

// --- Commands ---

#[tauri::command]
#[specta::specta]
pub async fn get_available_models(
    state: tauri::State<'_, ModelManagerState>,
) -> Result<Vec<ModelInfoDto>, CommandError> {
    let manager = state.lock().await;
    Ok(manager
        .list_all()
        .into_iter()
        .map(ModelInfoDto::from)
        .collect())
}

#[tauri::command]
#[specta::specta]
pub async fn download_model(
    state: tauri::State<'_, ModelManagerState>,
    window: tauri::Window,
    size: ModelSizeDto,
) -> Result<String, CommandError> {
    info!(size = ?size, "downloading model");
    // Clone the manager so we can release the lock before the long download.
    // ModelManager is cheap to clone (just a PathBuf).
    let manager = {
        let guard = state.lock().await;
        guard.clone()
    };
    let model_size: ModelSize = size.into();

    let path = manager
        .download(model_size, move |progress| {
            let _ = window.emit(
                "model-download-progress",
                serde_json::json!({
                    "percent": progress,
                    "size": format!("{:?}", model_size),
                }),
            );
        })
        .await
        .map_err(|e| {
            error!("model download failed: {}", e);
            CommandError::from(e)
        })?;

    info!(path = %path.display(), "model downloaded");
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command]
#[specta::specta]
pub async fn delete_model(
    state: tauri::State<'_, ModelManagerState>,
    size: ModelSizeDto,
) -> Result<(), CommandError> {
    info!(size = ?size, "deleting model");
    // Clone the manager so we can release the lock before async filesystem I/O.
    let manager = {
        let guard = state.lock().await;
        guard.clone()
    };
    manager.delete(size.into()).await.map_err(|e| {
        error!("model deletion failed: {}", e);
        CommandError::from(e)
    })
}

#[tauri::command]
#[specta::specta]
pub async fn shutdown_transcription_client(
    scheduler_state: tauri::State<'_, TranscriptionSchedulerState>,
    live_state: tauri::State<'_, super::live_transcription::LiveTranscriptionState>,
) -> Result<(), CommandError> {
    info!("shutting down transcription client");
    // Refuse to shut the engine down while a live runtime (session OR
    // dictation) is still attached.
    {
        let live_guard = live_state.lock().await;
        if live_guard.any_active() {
            return Err(CommandError::InvalidInput {
                message: "live transcription is running; stop it before \
                          shutting down the engine"
                    .into(),
            });
        }
    }
    let initialized = {
        let mut guard = scheduler_state.lock().await;
        guard.take()
    };
    if let Some(InitializedEngine { scheduler, .. }) = initialized {
        let timeout = std::time::Duration::from_secs(
            super::transcription_scheduler::DEFAULT_SHUTDOWN_TIMEOUT_SECS,
        );
        scheduler
            .shutdown_client(timeout)
            .await
            .map_err(|e| CommandError::Internal { message: e })?;
    }
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct TranscriptionStatusDto {
    pub initialized: bool,
}

/// Reports whether the transcription sidecar has been initialized.
///
/// The frontend uses this on mount to decide whether to call
/// `init_transcription_client` again. Without it, a Vite HMR remount re-runs
/// autoSetup and respawns the sidecar even when the backend is already warm,
/// leaving `enginePhase: "initializing"` long enough for dictation to fail
/// the readiness check.
#[tauri::command]
#[specta::specta]
pub async fn get_transcription_status(
    state: tauri::State<'_, TranscriptionSchedulerState>,
) -> Result<TranscriptionStatusDto, CommandError> {
    let guard = state.lock().await;
    Ok(TranscriptionStatusDto {
        initialized: guard.is_some(),
    })
}

// ---------- Engine catalogue + Parakeet/Sortformer commands ----------

/// Returns the static engine catalogue (Whisper + Parakeet capability + language lists).
/// The frontend uses this to drive the cascading engine → model → language UI.
#[tauri::command]
#[specta::specta]
pub async fn get_engine_catalogue() -> Result<Vec<EngineDescriptorDto>, CommandError> {
    Ok(engine_catalogue()
        .iter()
        .map(|d| EngineDescriptorDto {
            kind: d.kind.into(),
            display_name: d.display_name.to_string(),
            languages: d.languages.iter().map(|s| (*s).to_string()).collect(),
            supports_diarization: d.supports_diarization,
            supports_initial_prompt: d.supports_initial_prompt,
        })
        .collect())
}

#[tauri::command]
#[specta::specta]
pub async fn get_parakeet_models(
    state: tauri::State<'_, ModelManagerState>,
) -> Result<Vec<ParakeetModelInfoDto>, CommandError> {
    let manager = state.lock().await;
    Ok(ParakeetVariant::all()
        .iter()
        .copied()
        .map(|v| ParakeetModelInfoDto {
            variant: v.into(),
            downloaded: manager.parakeet_is_available(v),
            display_name: v.display_name().to_string(),
            approximate_size_bytes: v.approximate_size_bytes(),
        })
        .collect())
}

/// Returns the Parakeet variant the current host should run. Apple Silicon
/// gets the int8 bundle (smaller, no external `.onnx.data`, accelerator-
/// compatible); other targets keep the fp32 bundle.
///
/// The frontend uses this in two places:
///   1. As the source of truth for the v24 store migration that coerces
///      pre-existing `selectedParakeetVariant` values.
///   2. To render a single-variant Parakeet UI keyed to whatever the host
///      is meant to run, so users never have to pick int8 vs fp32.
#[tauri::command]
#[specta::specta]
pub async fn get_recommended_parakeet_variant() -> Result<ParakeetVariantDto, CommandError> {
    Ok(ParakeetVariant::recommended_for_host().into())
}

#[tauri::command]
#[specta::specta]
pub async fn download_parakeet_model(
    state: tauri::State<'_, ModelManagerState>,
    window: tauri::Window,
    variant: ParakeetVariantDto,
) -> Result<String, CommandError> {
    info!(variant = ?variant, "downloading parakeet model");
    let manager = {
        let guard = state.lock().await;
        guard.clone()
    };
    let v: ParakeetVariant = variant.into();
    let path = manager
        .download_parakeet(v, move |progress| {
            let _ = window.emit(
                "model-download-progress",
                serde_json::json!({
                    "percent": progress,
                    "kind": "parakeet",
                    "variant": format!("{:?}", v),
                }),
            );
        })
        .await
        .map_err(|e| {
            error!("parakeet model download failed: {}", e);
            CommandError::from(e)
        })?;
    info!(path = %path.display(), "parakeet model downloaded");
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command]
#[specta::specta]
pub async fn delete_parakeet_model(
    state: tauri::State<'_, ModelManagerState>,
    variant: ParakeetVariantDto,
) -> Result<(), CommandError> {
    info!(variant = ?variant, "deleting parakeet model");
    let manager = {
        let guard = state.lock().await;
        guard.clone()
    };
    manager
        .delete_parakeet(variant.into())
        .await
        .map_err(CommandError::from)
}

#[tauri::command]
#[specta::specta]
pub async fn get_sortformer_status(
    state: tauri::State<'_, ModelManagerState>,
) -> Result<SortformerModelInfoDto, CommandError> {
    let manager = state.lock().await;
    let v = SortformerVariant::V2_1;
    Ok(SortformerModelInfoDto {
        variant: SortformerVariantDto::V2_1,
        downloaded: manager.sortformer_model_path(v).is_some(),
        display_name: v.display_name().to_string(),
        approximate_size_bytes: v.approximate_size_bytes(),
    })
}

#[tauri::command]
#[specta::specta]
pub async fn download_sortformer_model(
    state: tauri::State<'_, ModelManagerState>,
    window: tauri::Window,
    variant: SortformerVariantDto,
) -> Result<String, CommandError> {
    info!(variant = ?variant, "downloading sortformer model");
    let manager = {
        let guard = state.lock().await;
        guard.clone()
    };
    let v: SortformerVariant = variant.into();
    let path = manager
        .download_sortformer(v, move |progress| {
            let _ = window.emit(
                "model-download-progress",
                serde_json::json!({
                    "percent": progress,
                    "kind": "sortformer",
                    "variant": format!("{:?}", v),
                }),
            );
        })
        .await
        .map_err(|e| {
            error!("sortformer download failed: {}", e);
            CommandError::from(e)
        })?;
    info!(path = %path.display(), "sortformer downloaded");
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command]
#[specta::specta]
pub async fn delete_sortformer_model(
    state: tauri::State<'_, ModelManagerState>,
    variant: SortformerVariantDto,
) -> Result<(), CommandError> {
    info!(variant = ?variant, "deleting sortformer model");
    let manager = {
        let guard = state.lock().await;
        guard.clone()
    };
    manager
        .delete_sortformer(variant.into())
        .await
        .map_err(CommandError::from)
}

/// Spawn the sidecar with the requested engine. Whisper requires
/// `whisper_model`; Parakeet requires `parakeet_variant` and may optionally
/// enable Sortformer diarization.
///
/// Re-init semantics:
/// - If the engine is uninitialized, build the client + scheduler.
/// - If the engine is already initialized with a *matching* config (same
///   kind + variant + diarization), return OK (idempotent — covers HMR
///   remounts that re-call init).
/// - If the engine is already initialized with a *different* config, return
///   an explicit error directing the caller to `shutdown_transcription_client`
///   first. Engine swap is an explicit two-step operation; no implicit drain.
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
pub async fn init_transcription_client(
    app_handle: tauri::AppHandle,
    model_manager_state: tauri::State<'_, ModelManagerState>,
    scheduler_state: tauri::State<'_, TranscriptionSchedulerState>,
    engine: EngineKindDto,
    whisper_model: Option<ModelSizeDto>,
    parakeet_variant: Option<ParakeetVariantDto>,
    enable_diarization: bool,
) -> Result<(), CommandError> {
    info!(
        engine = ?engine,
        whisper_model = ?whisper_model,
        parakeet_variant = ?parakeet_variant,
        enable_diarization,
        "initializing transcription client"
    );

    let requested = EngineConfig {
        kind: engine.into(),
        whisper_model: whisper_model.clone().map(Into::into),
        parakeet_variant: parakeet_variant.map(Into::into),
        diarization: enable_diarization,
    };

    {
        let guard = scheduler_state.lock().await;
        if let Some(existing) = guard.as_ref() {
            if existing.config == requested {
                info!("transcription client already initialized with matching config, skipping respawn");
                return Ok(());
            }
            return Err(CommandError::InvalidInput {
                message: "engine already initialized with a different config; \
                     call shutdown_transcription_client first"
                    .into(),
            });
        }
    }

    // Clone the model manager so ensure_vad_model / ensure_sortformer awaits
    // don't hold the outer ModelManagerState lock — those can take seconds
    // on a cold fetch and would otherwise serialize every other consumer.
    // ModelManager is cheap to clone (PathBuf-backed).
    let manager = {
        let guard = model_manager_state.lock().await;
        guard.clone()
    };
    let sidecar_path = find_sidecar_path()?;

    let (model_path, vad_model_path, sortformer_model_path, coreml_cache_dir) = match engine {
        EngineKindDto::Whisper => {
            let size: ModelSize = whisper_model
                .ok_or(CommandError::NotFound {
                    message: "whisper_model required when engine=Whisper".into(),
                })?
                .into();
            let mp = manager.model_path(size).ok_or(CommandError::NotFound {
                message: format!("whisper model {:?} not downloaded", size),
            })?;
            let vad = match manager.ensure_vad_model().await {
                Ok(p) => Some(p),
                Err(e) => {
                    error!("ensure_vad_model failed: {}, proceeding without VAD", e);
                    None
                }
            };
            (mp, vad, None, None)
        }
        EngineKindDto::Parakeet => {
            let variant: ParakeetVariant = parakeet_variant
                .ok_or(CommandError::NotFound {
                    message: "parakeet_variant required when engine=Parakeet".into(),
                })?
                .into();
            if !manager.parakeet_is_available(variant) {
                return Err(CommandError::NotFound {
                    message: format!("parakeet variant {:?} not downloaded", variant),
                });
            }
            let mp = manager.parakeet_model_dir(variant);
            let sortformer = if enable_diarization {
                Some(manager.ensure_sortformer(SortformerVariant::V2_1).await?)
            } else {
                None
            };
            let cache_dir = app_handle
                .path()
                .app_data_dir()
                .ok()
                .map(|d| d.join("cache").join("coreml"));
            (mp, None, sortformer, cache_dir)
        }
    };

    let engine_kind: EngineKind = engine.into();
    let client = TranscriptionClient::spawn(
        &sidecar_path,
        engine_kind,
        &model_path,
        vad_model_path.as_deref(),
        sortformer_model_path.as_deref(),
        coreml_cache_dir.as_deref(),
    )
    .await
    .map_err(|e| {
        error!("failed to spawn transcription client: {}", e);
        CommandError::from(e)
    })?;

    // Probe the sidecar for the model it just loaded (CLI-arg path
    // doesn't emit `model_loaded` over IPC) so we can surface accel +
    // variant to the FE. Best-effort: if the query fails, log and
    // continue — the client is still usable.
    let engine_info = match client.query_engine_info().await {
        Ok(info) => Some(info),
        Err(e) => {
            tracing::warn!(
                "query_engine_info failed after spawn ({e}); continuing without engine_loaded event"
            );
            None
        }
    };

    // Hand the client to a long-lived scheduler. The scheduler is the
    // single source of truth for engine readiness from this point on.
    let scheduler = super::transcription_scheduler::TranscriptionScheduler::new(Arc::new(client));
    {
        let mut guard = scheduler_state.lock().await;
        // Race-handling: two init calls can both pass the early `is_none`
        // check at the top of this command, both build clients, and both
        // reach here. The first one to take this lock wins. The runner-up
        // must distinguish two cases:
        //   - same config as the winner → idempotent OK (caller's intent
        //     is satisfied; drop the orphan scheduler).
        //   - different config → the caller asked for engine X but engine
        //     Y is now live. Returning Ok would lie about the engine
        //     state, so reject with the same "shut down first" error the
        //     non-racing path returns. This keeps init's contract single-
        //     valued: success means the engine running matches `requested`.
        let runner_up = match guard.as_ref() {
            None => false,
            Some(existing) => existing.config == requested,
        };
        if guard.is_none() {
            *guard = Some(InitializedEngine {
                config: requested,
                scheduler,
            });
        } else {
            // Lost the race. Drop the lock first, then shut down the
            // orphan scheduler we just built. The winner's scheduler is
            // already serving an engine — same config means idempotent
            // OK, different config means the caller's request is
            // incompatible with the live state.
            drop(guard);
            let timeout = std::time::Duration::from_secs(
                super::transcription_scheduler::DEFAULT_SHUTDOWN_TIMEOUT_SECS,
            );
            if let Err(e) = scheduler.shutdown_client(timeout).await {
                tracing::warn!("orphaned-scheduler shutdown error: {e}");
            }
            return if runner_up {
                Ok(())
            } else {
                Err(CommandError::InvalidInput {
                    message: "engine already initialized with a different config; \
                         call shutdown_transcription_client first"
                        .into(),
                })
            };
        }
    }
    info!(
        "transcription client initialized: engine={}",
        engine_kind.as_str()
    );

    if let Some(info) = engine_info {
        info!(
            marker = "live_engine_loaded",
            engine = engine_kind.as_str(),
            accel = info.accel.as_deref(),
            model_dir = info.model_dir.as_deref(),
            accel_fallback = info.accel_fallback.as_deref(),
            accel_partial = info.accel_partial,
            "transcription engine loaded"
        );
        let _ = app_handle.emit(
            "transcription-engine-loaded",
            TranscriptionEngineLoadedEvent {
                engine: engine_kind.into(),
                accel: info.accel,
                model_dir: info.model_dir,
                accel_fallback: info.accel_fallback,
                accel_partial: info.accel_partial,
            },
        );
    }
    Ok(())
}

/// Locates the sidecar binary relative to the current executable.
fn find_sidecar_path() -> Result<PathBuf, CommandError> {
    let exe_dir = std::env::current_exe()
        .map_err(|e| CommandError::Internal {
            message: format!("failed to get current exe: {e}"),
        })?
        .parent()
        .ok_or(CommandError::Internal {
            message: "failed to get exe directory".into(),
        })?
        .to_path_buf();

    // Tauri bundles sidecar binaries next to the main executable
    let target_triple = current_target_triple();
    let ext = if cfg!(target_os = "windows") {
        ".exe"
    } else {
        ""
    };
    let sidecar_name = format!("yapstack-transcription-sidecar-{target_triple}{ext}");

    let path = exe_dir.join(&sidecar_name);
    if path.exists() {
        return Ok(path);
    }

    // Fallback: try without target triple (development mode)
    let fallback_name = format!("yapstack-transcription-sidecar{ext}");
    let path = exe_dir.join(&fallback_name);
    if path.exists() {
        return Ok(path);
    }

    Err(CommandError::NotFound {
        message: format!(
            "sidecar binary not found at {} or {}",
            exe_dir.join(&sidecar_name).display(),
            exe_dir.join(&fallback_name).display()
        ),
    })
}

fn current_target_triple() -> &'static str {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "aarch64-apple-darwin"
        } else {
            "x86_64-apple-darwin"
        }
    } else if cfg!(target_os = "windows") {
        "x86_64-pc-windows-msvc"
    } else {
        "x86_64-unknown-linux-gnu"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_size_dto_roundtrip() {
        let sizes = [
            (ModelSizeDto::Tiny, ModelSize::Tiny),
            (ModelSizeDto::Base, ModelSize::Base),
            (ModelSizeDto::Small, ModelSize::Small),
            (ModelSizeDto::Medium, ModelSize::Medium),
        ];
        for (dto, expected) in sizes {
            let domain: ModelSize = dto.into();
            assert!(
                matches!(domain, ref e if std::mem::discriminant(&domain) == std::mem::discriminant(&expected))
            );
        }
    }

    #[test]
    fn test_model_size_domain_to_dto_roundtrip() {
        let sizes = [
            ModelSize::Tiny,
            ModelSize::Base,
            ModelSize::Small,
            ModelSize::Medium,
        ];
        for size in sizes {
            let dto: ModelSizeDto = size.into();
            let back: ModelSize = dto.into();
            assert_eq!(std::mem::discriminant(&size), std::mem::discriminant(&back));
        }
    }

    #[test]
    fn test_current_target_triple_not_empty() {
        let triple = current_target_triple();
        assert!(!triple.is_empty());
        assert!(triple.contains('-'), "target triple should contain hyphens");
    }
}
