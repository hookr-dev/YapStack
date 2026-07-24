use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{oneshot, Mutex};
use tracing::{debug, error, info, warn};
use yapstack_audio::manager::AudioManager;
use yapstack_audio::BufferPositions;
use yapstack_common::types::CaptureSource;

use super::error::{validate_session_id, CommandError};

use super::audio::{AudioManagerState, CaptureSourceDto, MixConfigDto};
use super::silero_vad::{SileroSource, SileroVad, SILENCE_THRESHOLD, SPEECH_THRESHOLD};
use super::transcription::{MicWindow, TranscriptSegmentDto, TranscriptionSchedulerState};
use super::transcription_scheduler::{
    source_from_label, JobOrigin, JobRequest, SchedulerError, TranscriptionScheduler,
    DEFAULT_SHUTDOWN_TIMEOUT_SECS,
};

// --- DTOs ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum AudioSourceLabel {
    Mic,
    System,
}

/// Routing identity of a live-transcription runtime. `Session` is the
/// long-running recording flow that writes session audio parts; `Dictation`
/// is the user-triggered, mic-only utterance flow whose segments must not
/// land in any session transcript. The frontend filters all live-
/// transcription events (segments, status, backfill-complete, etc.) on this
/// field rather than string-matching session ids.
///
/// This is *routing identity*, separate from `JobOrigin` which is the
/// scheduler's *priority class* (a dictation runtime can still emit
/// `FinalFlush`-class jobs at stop time). Frontend code that needs to know
/// "is this segment for the session transcript or the dictation history"
/// must read `source_kind`, never `origin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum LiveSourceKind {
    Session,
    Dictation,
}

impl LiveSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LiveSourceKind::Session => "session",
            LiveSourceKind::Dictation => "dictation",
        }
    }

    /// Toggle this routing identity's scheduler busy bit(s). The session
    /// loop covers mic + system in lockstep so it sets both bits; dictation
    /// owns only its own bit. Independent producer bits keep cross-loop
    /// busy state out of the backfill gate.
    fn set_scheduler_busy(
        self,
        scheduler: &super::transcription_scheduler::TranscriptionScheduler,
        busy: bool,
    ) {
        use super::transcription_scheduler::BusyKind;
        match self {
            LiveSourceKind::Session => scheduler.set_live_busy(busy),
            LiveSourceKind::Dictation => scheduler.set_busy(BusyKind::Dictation, busy),
        }
    }
}

fn default_source_kind() -> LiveSourceKind {
    LiveSourceKind::Session
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct LiveTranscriptionConfig {
    /// Milliseconds of continuous silence before triggering a chunk. Default: 800.
    pub silence_duration_ms: u32,
    /// Force a chunk after this many seconds of continuous speech. Default: 30.
    pub max_chunk_seconds: f32,
    /// Seconds of buffer lookback at session start (backfill). Default: 0.
    pub backfill_seconds: f32,
    /// Audio source.
    pub source: CaptureSourceDto,
    /// Mix config for `Mixed` source. `None` for `MicOnly` / `SystemOnly`,
    /// where mixing is undefined.
    pub mix_config: Option<MixConfigDto>,
    /// Whisper language.
    pub language: Option<String>,
    /// Max characters of prior transcript to feed as Whisper prompt context. Default: 350.
    pub prompt_context_chars: Option<u32>,
    /// Seconds of all-source silence before clearing prompt context to prevent
    /// hallucination from stale context. Default: 5.0. Set to 0 to disable.
    pub prompt_decay_silence_seconds: Option<f32>,
    /// Session ID for streaming session-audio recording. If set, the loop
    /// streams a new audio part to disk during the session and emits
    /// `session-part-ready` at finalize time. The on-disk file is
    /// `{audio_save_location || $APP_DATA_DIR/audio/}/{session_id}.{part_index}.{wav|mp3}`,
    /// where `part_index` is 0 for a fresh session and `N` when resuming a
    /// session that already has parts.
    pub session_id: Option<String>,
    /// When `true`, the finalize path inserts a `session_audio_parts` row
    /// keyed by `session_id` so the DB stays the durable source of truth even
    /// if the FE listener is gone. Set this to `false` for synthetic ids that
    /// are *not* rows in `sessions` — most importantly dictation, where the
    /// id is per-utterance and the finalized file is recorded against
    /// `dictation_history` instead. Defaults to `true` to preserve historical
    /// behavior for actual sessions; the dictation hook flips it off.
    #[serde(default = "default_persist_audio_part")]
    pub persist_audio_part: bool,
    /// Custom directory for saving session audio parts. If None, uses
    /// `$APP_DATA_DIR/audio/`. The directory is registered with
    /// `audio_save_locations` at recording start so reconciliation can
    /// recover orphan parts on the next startup.
    pub audio_save_location: Option<String>,
    /// Audio export format applied at part finalization. Default: `Mp3`
    /// (matches the legacy "no value provided" behaviour). Choosing `Mp3`
    /// re-encodes the streamed WAV at `mp3_bitrate` and deletes the source
    /// WAV; `Wav` keeps the streamed file as-is. Typed end-to-end so a stale
    /// or typo'd caller fails at deserialization rather than silently
    /// rerouting through the MP3 branch.
    pub audio_export_format: Option<AudioExportFormatDto>,
    /// MP3 bitrate in kbps (e.g. 64, 128, 192). Only used when format is "mp3".
    pub mp3_bitrate: Option<u16>,
    /// Request speaker diarization on every transcribed chunk. Honored only
    /// when the active engine is Parakeet *and* the sidecar was spawned with
    /// a Sortformer model path. Whisper sessions ignore this flag.
    #[serde(default)]
    pub diarization: bool,
    /// Comma-separated vocabulary hints (folder/tag names) prepended to the
    /// Whisper initial_prompt to improve recognition of proper nouns. Ignored
    /// for Parakeet sessions (the TDT decoder has no text-prompt input).
    pub vocabulary_hints: Option<String>,
    /// When set, this run is a resume of an existing Session. The new
    /// recording becomes a fresh audio part appended after the existing parts;
    /// no prior file is read or modified. Backfill is forced to 0.
    #[serde(default)]
    pub resume: Option<ResumeConfig>,
    /// Routing identity of this runtime. Defaults to `Session` so existing
    /// callers keep working without TS-side changes during a transitional
    /// build; the dictation hook must pass `Dictation` explicitly.
    #[serde(default = "default_source_kind")]
    pub source_kind: LiveSourceKind,
}

fn default_persist_audio_part() -> bool {
    true
}

/// Typed surface for the audio finalize format. Lowercase serde tags match
/// the legacy `"wav"` / `"mp3"` strings the FE already passes and the
/// `format` field on `SessionPartReadyEvent` already emits, so this is a
/// drop-in tightening of the Tauri boundary — generated TypeScript becomes
/// a `"wav" | "mp3"` discriminated union instead of `string | null`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum AudioExportFormatDto {
    Wav,
    Mp3,
}

impl AudioExportFormatDto {
    pub fn is_mp3(self) -> bool {
        matches!(self, AudioExportFormatDto::Mp3)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AudioExportFormatDto::Wav => "wav",
            AudioExportFormatDto::Mp3 => "mp3",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ResumeConfig {
    /// The index of the new part being recorded — equals the count of
    /// existing parts in the session. The output WAV/MP3 is named
    /// `{session_id}.{part_index}.{ext}`.
    pub part_index: u32,
    /// Cumulative duration of the existing parts. Added to every Segment's
    /// `audio_offset_seconds` so persisted Segments stay continuous.
    pub offset_base_seconds: f32,
}

#[derive(Debug, Clone, Serialize, Type)]
pub enum LiveTranscriptionPhase {
    Running,
    Stopped,
    Error,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct LiveTranscriptionStatus {
    pub phase: LiveTranscriptionPhase,
    pub chunks_processed: u32,
    pub total_audio_seconds: f32,
    pub error_message: Option<String>,
    pub session_id: Option<String>,
    pub effective_start_epoch_ms: Option<f64>,
    /// Session-time elapsed since session start minus the latest completed
    /// chunk's end offset. Rising values mean transcription is falling behind
    /// real time; falling values mean the consumer is catching up. None until
    /// the first successful chunk lands.
    pub lag_seconds: Option<f32>,
    /// Count of live chunks that left already-captured audio to drain after
    /// dispatching one max-duration slice. Non-zero means the sidecar fell
    /// behind real time, but audio is being preserved rather than dropped.
    pub live_drain_backlog_chunks: u32,
    /// Latest backlog depth after a live max-duration slice was dispatched.
    /// Returns to 0.0 once the live force-drain path catches up.
    pub live_drain_backlog_seconds: f32,
    /// Routing identity of the runtime that emitted this status. Listeners
    /// in the frontend filter by this so a dictation runtime's `Stopped`
    /// or `Error` does not flow into the session UI's phase machine.
    pub source_kind: LiveSourceKind,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct LiveTranscriptionStartResult {
    pub effective_start_epoch_ms: f64,
}

/// Origin class of a live-segment event. Mirrors the scheduler's priority
/// tier so the frontend can bucket segments (live vs. backfill stripe vs.
/// final-flush at stop).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SegmentOrigin {
    Live,
    Backfill,
    FinalFlush,
}

impl From<super::transcription_scheduler::JobOrigin> for SegmentOrigin {
    fn from(o: super::transcription_scheduler::JobOrigin) -> Self {
        use super::transcription_scheduler::JobOrigin;
        match o {
            // `Dictation` is a *priority class* on the scheduler side, not a
            // routing identity on the segment side. A dictation runtime's
            // chunks are tagged `JobOrigin::Dictation` for queue priority,
            // but the resulting segments still ride the live priority lane
            // semantically — the frontend routes by `source_kind`, never by
            // `origin`. Mapping `Dictation → Live` here keeps `origin`
            // honest as a priority indicator without inviting routing-by-
            // origin mistakes downstream.
            JobOrigin::Live | JobOrigin::Dictation => SegmentOrigin::Live,
            JobOrigin::Backfill => SegmentOrigin::Backfill,
            JobOrigin::FinalFlush => SegmentOrigin::FinalFlush,
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct LiveSegmentEvent {
    pub chunk_index: u32,
    /// Which audio source produced this chunk.
    pub source: AudioSourceLabel,
    /// Segments from this chunk.
    pub segments: Vec<TranscriptSegmentDto>,
    /// Offset in seconds from the start of live transcription.
    pub audio_offset_seconds: f32,
    /// Duration of this chunk's audio in seconds.
    pub chunk_duration_seconds: f32,
    /// Per-source accumulated text so far.
    pub accumulated_text: String,
    /// Origin class: `live`, `backfill`, or `final_flush`. Set by the scheduler
    /// at emit time. Lets the frontend bucket segments by priority class.
    /// Strictly distinct from `source_kind` (routing identity) — never use
    /// this field to decide whether a segment belongs to a session vs a
    /// dictation. A dictation runtime's final-flush is still `final_flush`.
    pub origin: SegmentOrigin,
    /// Routing identity of the runtime that produced this segment. The
    /// frontend filters segments destined for the session transcript vs the
    /// dictation history on this field. Defaulted to `Session` so old
    /// payloads still deserialize during a transitional build.
    #[serde(default = "default_source_kind")]
    pub source_kind: LiveSourceKind,
    /// Session this chunk belongs to. Carried on the event so late-arriving
    /// segments can still be persisted to the right session even if the
    /// frontend has already cleared `activeSessionId` during finalization.
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct SessionPartReadyEvent {
    pub session_id: String,
    pub part_index: u32,
    pub file_path: String,
    /// Always emitted as the typed enum (`"wav"` or `"mp3"`). Listeners can
    /// branch on this without string-literal guards.
    pub format: AudioExportFormatDto,
    pub duration_seconds: f32,
    pub sample_rate: u32,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct SessionWavErrorEvent {
    pub session_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct SessionWavWarningEvent {
    pub session_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct LiveTranscriptionWarningEvent {
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct StreamHealthEvent {
    pub source: AudioSourceLabel,
    /// "restarted" | "restart_failed" | "restart_abandoned"
    pub status: String,
    pub message: String,
    /// Human-readable name of the device the Stream is bound to after the
    /// event. Set on successful auto-failover so the FE can render
    /// "Switched to {name}" toasts. `None` for failures or when the
    /// underlying capture didn't report a device name.
    #[serde(default)]
    pub bound_device_name: Option<String>,
}

/// Per-chunk timing telemetry. Emitted after every transcribe attempt (success
/// OR failure) so the frontend and logs can see when the pipeline is falling
/// behind real time. `wall_ms / (chunk_audio_seconds * 1000) = RTFx`; values
/// below 1.0 mean the consumer is slower than real time and lag will grow.
#[derive(Debug, Clone, Serialize, Type)]
pub struct LiveTranscriptionPressureEvent {
    pub source: AudioSourceLabel,
    pub chunk_index: u32,
    pub chunk_audio_seconds: f32,
    pub wall_ms: u64,
    /// `chunk_audio_seconds / (wall_ms / 1000)`. None when transcribe failed
    /// or wall_ms is 0.
    pub rtfx: Option<f32>,
    /// Engine that produced this chunk — `"Whisper"` or `"Parakeet"`.
    pub engine: String,
    /// Priority tier the scheduler ran this chunk at. Lets pressure
    /// telemetry distinguish live throughput from backfill drain rate.
    pub origin: SegmentOrigin,
    /// Session-time elapsed since session start minus the just-completed
    /// chunk's end offset. Positive means "transcription is N seconds behind
    /// real time at the moment this chunk finished." None when the chunk did
    /// not produce a successful Transcription response.
    pub lag_seconds: Option<f32>,
    /// Backlog that remained queued for this source immediately after this
    /// live chunk was dispatched. Non-zero means live transcription is
    /// draining preserved audio instead of dropping it.
    pub drain_backlog_seconds: f32,
    /// Resolved accelerator (`"webgpu"`, `"coreml"`, `"cuda"`, `"cpu"`,
    /// `"metal"`) for the active sidecar. Captured once at session start
    /// from the client's cached engine info — rises with the rest of the
    /// pressure payload so a single grep'd `live_pressure` line tells us
    /// whether a slow chunk happened on GPU or CPU.
    pub accel: Option<String>,
    /// For Parakeet, the variant directory name (e.g.
    /// `"parakeet-tdt-v3-int8"` or `"parakeet-tdt-v3"`) so we can tell
    /// int8 vs fp32 sessions apart in logs without joining against
    /// `live_engine_loaded`. None for Whisper (single bundle) or when
    /// the sidecar didn't report a model_dir.
    pub variant: Option<String>,
}

/// Internal state for streaming WAV recording during a live session.
struct SessionWavState {
    writer: yapstack_audio::SessionWavWriter,
    flush_positions: BufferPositions,
    source: CaptureSource,
    mix_config: Option<yapstack_audio::MixConfig>,
    session_id: String,
    /// Index of this part within its session (0 for fresh, N for resume).
    part_index: u32,
    flush_count: u32,
    /// Sample rate the WAV file's header was opened with. If a source's
    /// ring buffer gets replaced at a different sample rate mid-session
    /// (device format change), subsequent extracted samples are resampled
    /// to this rate before `write_samples` so the archived file plays at
    /// a single consistent speed.
    wav_sample_rate: u32,
    /// When `true`, finalize inserts a `session_audio_parts` row keyed by
    /// `session_id`. Set to `false` for dictation, whose `session_id` is a
    /// synthetic per-utterance value with no `sessions` row backing it.
    persist_audio_part: bool,
}

// --- Shared context ---

/// Immutable shared context for transcription operations.
///
/// The `TranscriptionScheduler` is constructed once at engine init and
/// outlives every live runtime. Both the session and dictation runtimes
/// hold cloned `Arc<Scheduler>` handles and submit into the same single-
/// worker queue; the scheduler's `JobOrigin::Dictation` priority tier
/// keeps dictation chunks ahead of session live chunks.
#[derive(Clone)]
struct TranscriptionContext {
    /// Long-lived priority-queue scheduler in front of the sidecar lane.
    /// Cloned by every live runtime; the cheap `Clone` of the context
    /// shares the same underlying scheduler.
    scheduler: Arc<TranscriptionScheduler>,
    app_handle: AppHandle,
    config: LiveTranscriptionConfig,
    /// Shared prompt context bridging backfill → live transcription.
    bridged_prompt: Arc<Mutex<String>>,
    /// Dynamic vocabulary hints updated by frontend during recording.
    vocabulary_hints: Arc<Mutex<String>>,
    /// Constant added to every Segment's `audio_offset_seconds` so resumed
    /// runs produce offsets continuous with prior parts. Zero on a fresh
    /// Session, equals SUM of prior part durations on a resumed Session.
    session_offset_base_seconds: f32,
    /// Wall-clock instant the live loop started. Combined with
    /// `session_offset_base_seconds` this gives "session-time now" for lag
    /// calculations: lag = (offset_base + start.elapsed()) - latest_completed_audio_offset.
    session_start_instant: Instant,
    /// Engine-conditional configuration resolved once when the session
    /// starts. Live loop, `transcribe_chunk`, and `run_prompt_decay`
    /// consult this instead of branching on `EngineKind` at every site.
    /// `Arc` so the cheap `Clone` on `TranscriptionContext` doesn't
    /// re-allocate the underlying tuning struct.
    engine_profile: Arc<EngineProfile>,
    /// Mic-ownership coordination shared with any concurrent dictation
    /// runtime. Cloned-Arc so both runtimes' `TranscriptionContext` clones
    /// observe the same window list.
    dictation_owns_mic: super::transcription::DictationOwnsMicState,
    /// For dictation runtimes, the mic ring-buffer write position
    /// captured at the moment the dictation runtime opened its window.
    /// Used to initialize the dictation runtime's mic VAD cursors and
    /// session-WAV flush position so audio captured between the hotkey
    /// press and the live loop spawn is included in the dictation
    /// transcript and WAV. `None` for session runtimes.
    dictation_start_pos: Option<u64>,
}

/// Result of transcribing a single chunk.
struct ChunkResult {
    event: LiveSegmentEvent,
    chunk_duration: f32,
    /// Wall-time the sidecar took to process this chunk (round-trip
    /// `transcribe_with` duration). Tracked here so the success path can
    /// fold it into `SessionCounters::total_wall_ms` without re-measuring.
    wall_ms: u64,
}

/// Outcome of a transcription attempt — determines cursor management and
/// loop control. Too-short and empty extractions are filtered before dispatch
/// in `prepare_chunk_dispatch`, so we don't need a TooShort variant here.
enum TranscribeOutcome {
    /// Transcription succeeded with segments.
    Success(ChunkResult),
    /// Transcription failed non-fatally (temp WAV write error, engine error,
    /// timeout). The fire-and-forget dispatch path quarantines the audio
    /// immediately — retries are not attempted in the concurrent design.
    Skipped,
    /// Sidecar is dead and could not be restarted — main loop exits.
    SidecarDead,
}

/// RAII guard that deletes a temp file on drop (including panic unwind).
struct TempFileGuard(std::path::PathBuf);
impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Read-only input data for a single transcription chunk.
struct ChunkInput<'a> {
    samples: &'a [f32],
    sample_rate: u32,
    audio_offset_seconds: f32,
    source_label: AudioSourceLabel,
    drain_backlog_seconds: f32,
    /// Priority class the scheduler uses; also emitted on `LiveSegmentEvent`
    /// so the frontend can bucket segments.
    origin: JobOrigin,
}

/// Cross-loop chunk counters surfaced to `LiveTranscriptionController` for
/// status reporting. Shared between the live loop and `process_backfill` so
/// `chunks_processed` / `total_audio_seconds` reflect both sources of work.
///
/// All mutation sites lock briefly; the mutex is never held across an await.
struct SessionCounters {
    total_chunks: u32,
    total_audio_seconds: f32,
    /// Count of live dispatches that had more already-captured audio waiting
    /// after one max-size slice was submitted.
    live_drain_backlog_chunks: u32,
    /// Latest source-local live backlog depth after dispatch. This returns to
    /// zero when a later live chunk catches up.
    live_drain_backlog_seconds: f32,
    /// Cumulative wall-time across every successful transcribe — for an
    /// aggregate "session RTFx" view if we want one later.
    total_wall_ms: u64,
    /// Highest session-time end (`audio_offset + chunk_duration`) we have
    /// ever successfully transcribed. Monotonic — we take the max rather
    /// than overwriting on every chunk because backfill and live can land
    /// out of audio-time order. Used by `get_live_transcription_status` to
    /// compute lag against the current wall clock. None until the first
    /// chunk lands.
    latest_completed_audio_offset_seconds: Option<f32>,
}

/// Per-loop prompt accumulator. The live loop and `process_backfill` each own
/// their own instance — they must not share, because backfill bridges its
/// final accumulated prompt out to `bridged_prompt`, and a concurrent live
/// chunk writing into the same buffer would either get clobbered by the
/// bridge or cause the bridge to ship live text back into `bridged_prompt`.
///
/// Shared among concurrent per-source live chunk tasks via `Arc<StdMutex<_>>`.
/// All mutation sites lock briefly; the mutex is never held across an await.
struct PromptState {
    shared_prompt: String,
    /// When the last successful transcription occurred. Used for prompt decay —
    /// if no transcription has happened for `prompt_decay_silence_seconds`, all
    /// prompt context is cleared to prevent stale text from causing hallucinations.
    last_transcription_at: Option<Instant>,
}

/// One source's backfill data: the raw samples plus VAD-simulated chunk
/// boundaries (start/end sample indices) produced by `vad_chunk_historical_audio`.
struct VadBackfillSource {
    label: AudioSourceLabel,
    samples: Vec<f32>,
    sample_rate: u32,
    chunks: Vec<VadBackfillChunk>,
}

// --- Controller ---

pub(crate) struct LiveTranscriptionController {
    task_handle: tokio::task::JoinHandle<()>,
    /// Two coupled stop signals. `stop_tx` carries the `StopRequest` payload
    /// (positions + tail grace) — the loop must consume it exactly when it's
    /// ready to break, and `oneshot` is single-use, so this is the
    /// data-bearing channel. `stop_requested` is checked between `await`
    /// points inside the loop body via `stop_if_requested`; without it, a
    /// stop arriving mid-body wouldn't be observed until the next
    /// `tokio::select!` at the top of the loop (one tick late).
    /// `stop_live_transcription` sets the atomic *first* (so any in-body
    /// checkpoint sees it) then sends via the channel (so the eventual
    /// receiver gets the payload).
    stop_tx: Option<oneshot::Sender<StopRequest>>,
    stop_requested: Arc<AtomicBool>,
    session_id: Option<String>,
    effective_start_epoch_ms: f64,
    /// Live counters shared with the running loop and backfill task. They
    /// mutate in place when a chunk lands; `get_live_transcription_status`
    /// reads them via a brief lock to surface real values to the UI.
    counters: Arc<StdMutex<SessionCounters>>,
    /// Mirrors `TranscriptionContext::session_start_instant` so the polled
    /// status command can compute `lag_seconds` without having to reach into
    /// the running loop's private state.
    session_start_instant: Instant,
    /// Mirrors `TranscriptionContext::session_offset_base_seconds` for the
    /// same reason — needed to convert wall-clock elapsed into session-time.
    session_offset_base_seconds: f32,
}

impl LiveTranscriptionController {
    pub fn is_running(&self) -> bool {
        !self.task_handle.is_finished()
    }

    /// Snapshot the live counters and derive `lag_seconds` against the wall
    /// clock under a single short lock. `lag_seconds` is `None` until the
    /// first successful chunk has landed.
    fn snapshot(&self) -> (u32, f32, Option<f32>, u32, f32) {
        let s = self.counters.lock().expect("counters mutex poisoned");
        let lag = s.latest_completed_audio_offset_seconds.map(|chunk_end| {
            let session_time_now = self.session_offset_base_seconds
                + self.session_start_instant.elapsed().as_secs_f32();
            (session_time_now - chunk_end).max(0.0)
        });
        (
            s.total_chunks,
            s.total_audio_seconds,
            lag,
            s.live_drain_backlog_chunks,
            s.live_drain_backlog_seconds,
        )
    }
}

/// RAII guard that reverts a runtime slot from `Starting` back to `Idle`
/// on Drop unless `disarm()` was called first. Lets `start_live_transcription`
/// reserve its slot under a brief lock, drop the lock during fallible
/// setup (preflight / audio reads / scheduler lookup), and only re-acquire
/// the lock at the end to install `Running`. If any setup step `?`s out,
/// the guard's Drop spawns a detached task to take the live-state lock
/// asynchronously and revert the slot — Drop is sync, the revert needs
/// an async lock, so spawn-detached is the only shape that works.
///
/// Safe against racing same-kind starts because the slot guard rejects
/// non-`Idle` slots, including `Starting`. No racing start can install
/// `Running` in the gap between the failing call's Drop and the spawned
/// revert task's lock acquisition.
struct StartingSlotRevertGuard {
    state: LiveTranscriptionState,
    kind: LiveSourceKind,
    armed: bool,
}

impl StartingSlotRevertGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StartingSlotRevertGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let state = self.state.clone();
        let kind = self.kind;
        tokio::spawn(async move {
            let mut slots = state.lock().await;
            // Only revert if still Starting — defensive against a
            // future code path that might transition Starting → some
            // other state directly. Today nothing does, but the check
            // costs nothing and keeps the contract narrow.
            if matches!(slots.slot(kind), RuntimeSlot::Starting) {
                *slots.slot_mut(kind) = RuntimeSlot::Idle;
            }
        });
    }
}

/// RAII safety net for the dictation runtime's open mic-ownership window.
/// The dictation task's finalizer is the canonical close: it reads the
/// final mic position and calls `close(end_pos)` to record an accurate
/// `[start, end)` window the session can consume. This guard only fires
/// for paths the finalizer doesn't reach — early errors after the window
/// was opened but before the loop spawned, or app-exit dropping the slot
/// without the spawned task completing. Both cases mean no audio was
/// actually transcribed, so we use `clear_open()` to drop the open state
/// without recording a window. Drop after a normal close is a no-op
/// (state.open is already None).
pub struct MicOwnershipGuard {
    state: super::transcription::DictationOwnsMicState,
}

impl Drop for MicOwnershipGuard {
    fn drop(&mut self) {
        self.state.clear_open();
    }
}

pub struct LiveTranscriptionRuntime {
    pub(crate) controller: LiveTranscriptionController,
    vocabulary_hints: Arc<Mutex<String>>,
    /// Routing identity of this runtime — used by status/stop callsites to
    /// re-emit the right `source_kind` on synthetic events and to verify
    /// stop calls target the right runtime.
    pub(crate) source_kind: LiveSourceKind,
    /// Belt-and-suspenders mic-ownership clear. Only populated for the
    /// dictation runtime; `None` for sessions. The primary clear is
    /// task-finalizer-driven (so dictation's tail/final-flush can still
    /// read mic samples while the flag is true); this guard catches paths
    /// the finalizer might miss (early error before spawn, runtime drop
    /// at app exit).
    #[allow(dead_code)]
    mic_ownership_guard: Option<MicOwnershipGuard>,
}

/// Lifecycle state of one runtime slot. Stays non-`Idle` for the full
/// lifetime including finalization — `Stopping` is the window between
/// `stop_live_transcription` requesting shutdown and the spawned task's
/// finalizer running. Same-kind start during `Stopping` is rejected; the
/// finalizer transitions the slot back to `Idle` once the loop is fully
/// drained.
pub enum RuntimeSlot {
    Idle,
    /// `start_live_transcription` accepted and is mid-construction. The
    /// slot is held here so racing same-kind starts are rejected before
    /// the runtime is fully wired. Today the build is synchronous within
    /// the command so the visible window is vanishingly small; kept so
    /// the state machine has no ambiguous gaps if construction ever moves
    /// across an await.
    Starting,
    Running(LiveTranscriptionRuntime),
    Stopping(LiveTranscriptionRuntime),
}

impl RuntimeSlot {
    pub fn is_idle(&self) -> bool {
        matches!(self, RuntimeSlot::Idle)
    }

    pub fn runtime(&self) -> Option<&LiveTranscriptionRuntime> {
        match self {
            RuntimeSlot::Running(r) | RuntimeSlot::Stopping(r) => Some(r),
            _ => None,
        }
    }
}

pub struct LiveTranscriptionSlots {
    pub session: RuntimeSlot,
    pub dictation: RuntimeSlot,
}

impl LiveTranscriptionSlots {
    pub fn new() -> Self {
        Self {
            session: RuntimeSlot::Idle,
            dictation: RuntimeSlot::Idle,
        }
    }

    pub fn slot(&self, kind: LiveSourceKind) -> &RuntimeSlot {
        match kind {
            LiveSourceKind::Session => &self.session,
            LiveSourceKind::Dictation => &self.dictation,
        }
    }

    pub fn slot_mut(&mut self, kind: LiveSourceKind) -> &mut RuntimeSlot {
        match kind {
            LiveSourceKind::Session => &mut self.session,
            LiveSourceKind::Dictation => &mut self.dictation,
        }
    }

    /// True when any slot is non-`Idle` — used by `shutdown_transcription_client`
    /// and by lifecycle callers that need a "is anything live?" signal.
    pub fn any_active(&self) -> bool {
        !self.session.is_idle() || !self.dictation.is_idle()
    }
}

impl Default for LiveTranscriptionSlots {
    fn default() -> Self {
        Self::new()
    }
}

pub type LiveTranscriptionState = Arc<Mutex<LiveTranscriptionSlots>>;

/// Inbox for cross-thread "please restart this Source" requests, used by
/// the device broker (`device_broker` module) when a Core Audio default
/// device change requires re-binding a Stream. Set to `Some(sender)` for
/// the lifetime of an active live-transcription session, `None`
/// otherwise. The broker checks the inbox before deciding whether to
/// route a restart through the live loop (which knows how to reset
/// `SourceVadState`) or to call `AudioManager::restart_*` directly.
pub type RestartIntentSender = tokio::sync::mpsc::UnboundedSender<RestartIntent>;
pub type RestartIntentInbox = Arc<StdMutex<Option<RestartIntentSender>>>;

/// Cross-task signal: `true` while a live transcription loop is running
/// or in its stop-bounded final-flush tail; `false` only when no live
/// loop owns audio cursors / ring buffers.
///
/// The device broker reads this to decide whether a device-change event
/// should route through the [`RestartIntentInbox`] (when `true`) or be
/// dispatched directly via `AudioManager::restart_*` (when `false`).
/// Inbox-presence alone is not sufficient because `stop_live_transcription`
/// clears the inbox before the loop has finished its final flush; a
/// device-change event in that window would otherwise see "no inbox" and
/// race the loop's snapshotted stop positions by replacing the ring
/// buffer mid-finalize.
///
/// The flag is set `true` by `start_live_transcription` before spawning
/// the loop and cleared `false` by the spawned task itself once the loop
/// has fully returned (after scheduler shutdown). This bridges the
/// stop/finalize window.
pub type LiveSessionPresent = Arc<AtomicBool>;

/// What the broker is asking the live loop to do. Narrow on purpose —
/// the loop doesn't need a "target device id" because it always rebinds
/// to the current OS default (the broker has already debounced and
/// confirmed liveness via `device_liveness`).
#[derive(Debug, Clone, Copy)]
pub enum RestartIntent {
    Mic,
    System,
}

struct StopRequest {
    positions: BufferPositions,
}

// --- VAD helpers ---

/// Per-source voice activity detection state.
struct SourceVadState {
    /// Which audio source this tracks.
    label: AudioSourceLabel,
    is_speaking: bool,
    /// Scalar buffer position when speech started (extraction start point).
    speech_start_pos: usize,
    /// Scalar read cursor — advances as we extract audio.
    cursor: usize,
    /// When speech started (for max-duration enforcement).
    speech_start_time: Option<Instant>,
    /// When energy last dropped below threshold (start of silence).
    silence_since: Option<Instant>,
    /// Per-source chunk counter.
    chunk_index: u32,
    /// Per-source accumulated transcription text.
    accumulated_text: String,
    /// Buffer position corresponding to T=0 on the session timeline (rewound backfill position).
    session_start_pos: usize,
    /// Session-time (in seconds) corresponding to this source's current
    /// `session_start_pos`. At source construction this equals
    /// `ctx.session_offset_base_seconds`; on `reset_for_buffer_replacement`
    /// it advances by the wall-clock time elapsed since the last reset/resume
    /// so that the chunk-offset formula
    /// `audio_offset = anchor + (start_pos - session_start_pos)/sr`
    /// stays continuous across ring-buffer swaps. Read at the chunk-emit
    /// site in `prepare_chunk_dispatch_inner` and in the stop-time
    /// `copy_final_pending_chunks_until` helper. Never used in lag
    /// calculations — those keep using `ctx.session_offset_base_seconds`
    /// (session-wide, immutable) plus `session_start_instant.elapsed()`
    /// (wall-clock).
    audio_offset_anchor_seconds: f32,
    /// Sample rate of this source's ring buffer.
    source_sample_rate: u32,
    /// Channel count of this source's ring buffer (interleaved).
    source_channels: u16,
    /// Last observed write_pos for stall detection.
    last_seen_write_pos: usize,
    /// When last_seen_write_pos last advanced.
    last_write_pos_advance: Instant,
    /// Number of restart attempts for this source.
    restart_attempts: u32,
    /// When the last restart was attempted (for cooldown).
    last_restart_at: Option<Instant>,
    /// True while a background chunk task is running for this source. VAD
    /// state keeps updating (so second-utterance-during-in-flight is still
    /// captured) but dispatch and idle-cursor advance are gated off.
    /// Cleared when the task completes and the main loop applies its outcome.
    has_in_flight_task: bool,
    /// Lower bound for onset pre-roll adjustments. Prevents pre-roll from
    /// pulling `speech_start_pos` before the last dispatched chunk's end,
    /// which would cause the next chunk to re-transcribe the tail of the
    /// previous one. Set to `new_pos` at every chunk dispatch and to
    /// `session_start_pos` at loop entry (no prior dispatch to respect).
    earliest_next_chunk_pos: usize,
    /// Silero VAD streaming state and VAD-only read cursor for this source.
    /// Populated regardless of engine — Silero replaces RMS for both
    /// Whisper and Parakeet live sessions.
    silero: SileroSource,
    /// True when a previous dispatch intentionally sent only the oldest
    /// max-sized slice and there is more already-captured audio to drain.
    force_drain: bool,
    /// Mic ring-buffer position past which every dictation window this
    /// source has seen has already been consumed (cursors advanced, VAD
    /// reset). Compared against each closed window's `end` on every
    /// session tick: a window with `end > dictation_consumed_through`
    /// is new and needs processing. Position-based instead of index-
    /// based so the closed-window list can be pruned (entries with
    /// `end <= min_consumer_pos` are dropped from the front) without
    /// invalidating this consumer's progress. Always 0 for non-Mic
    /// sources and for non-Session runtimes.
    dictation_consumed_through: u64,
    /// Start position of the *open* dictation window we've already pre-
    /// flushed for. Set when a Mic-Session source observes
    /// `snapshot.open == Some(start)` for the first time, so each open
    /// window only triggers a single pre-window dispatch even if the
    /// session ticks many times before the close lands. None when no
    /// open window has been observed yet, or after the matching closed
    /// window has been consumed.
    flushed_open_at: Option<u64>,
}

impl SourceVadState {
    fn new(
        label: AudioSourceLabel,
        initial_pos: usize,
        session_start_pos: usize,
        sample_rate: u32,
        channels: u16,
        audio_offset_anchor_seconds: f32,
    ) -> Self {
        Self {
            has_in_flight_task: false,
            earliest_next_chunk_pos: session_start_pos,
            label,
            is_speaking: false,
            speech_start_pos: initial_pos,
            cursor: initial_pos,
            speech_start_time: None,
            silence_since: None,
            chunk_index: 0,
            accumulated_text: String::new(),
            session_start_pos,
            audio_offset_anchor_seconds,
            source_sample_rate: sample_rate,
            source_channels: channels,
            last_seen_write_pos: initial_pos,
            last_write_pos_advance: Instant::now(),
            restart_attempts: 0,
            last_restart_at: None,
            silero: SileroSource::new(initial_pos),
            force_drain: false,
            dictation_consumed_through: 0,
            flushed_open_at: None,
        }
    }

    /// Rewinds VAD progress past `new_pos`. Used when a contiguous range
    /// up to `new_pos` should be excluded from any future session
    /// transcription decision (today: closing a dictation mic-ownership
    /// window). Cursors jump forward, in-flight speech state drops, and
    /// the Silero stream is reset so the next sample is treated as
    /// fresh. Preserves cross-window state: chunk_index, accumulated_text,
    /// session_start_pos, source format, restart counters.
    fn reset_vad_past(&mut self, new_pos: usize) {
        self.cursor = new_pos;
        self.speech_start_pos = new_pos;
        self.earliest_next_chunk_pos = new_pos;
        self.silero.read_pos = new_pos;
        self.silero.reset();
        self.is_speaking = false;
        self.speech_start_time = None;
        self.silence_since = None;
        self.force_drain = false;
    }

    /// Resets all buffer-position, VAD, and source-format state after the
    /// underlying ring buffer has been replaced (e.g. on a device-format
    /// change during restart). Positions from the old buffer are not
    /// meaningful against the new one, and the sample rate / channel count
    /// must track the new device's format so extraction and WAV writes stay
    /// correct.
    ///
    /// Also re-anchors `audio_offset_anchor_seconds` to `new_anchor` so the
    /// chunk-offset formula stays continuous with wall-clock session time —
    /// without this, the post-swap `session_start_pos` would map to the
    /// old anchor (typically `ctx.session_offset_base_seconds`), silently
    /// rewinding every subsequent chunk's offset by however many seconds
    /// of wall clock have elapsed inside the current loop. Callers compute
    /// `new_anchor` as `ctx.session_offset_base_seconds + ctx.session_start_instant.elapsed()`.
    ///
    /// Preserves cross-buffer state: `chunk_index`, `accumulated_text`.
    /// `dictation_consumed_through` and `flushed_open_at` are caller-managed
    /// (reset in `handle_buffer_replacement` for mic sources) — they are
    /// ring-buffer positions and would also be invalid against the new
    /// buffer, but only the live loop knows whether this source is the
    /// mic in a Session runtime, so the policy lives there.
    fn reset_for_buffer_replacement(
        &mut self,
        new_pos: usize,
        sample_rate: u32,
        channels: u16,
        new_anchor: f32,
    ) {
        self.is_speaking = false;
        self.speech_start_pos = new_pos;
        self.cursor = new_pos;
        self.speech_start_time = None;
        self.silence_since = None;
        self.session_start_pos = new_pos;
        self.audio_offset_anchor_seconds = new_anchor;
        self.source_sample_rate = sample_rate;
        self.source_channels = channels;
        self.last_seen_write_pos = new_pos;
        self.last_write_pos_advance = Instant::now();
        self.earliest_next_chunk_pos = new_pos;
        self.silero = SileroSource::new(new_pos);
        self.force_drain = false;
    }
}

/// Describes what action the VAD state machine wants to take.
enum VadAction {
    /// No action needed — continue polling.
    None,
    /// Speech pause detected — chunk and transcribe.
    Chunk,
    /// Max duration exceeded while still speaking — force chunk.
    ForceChunk,
}

/// Per-engine tuning for the VAD / chunking loop.
///
/// The detector itself (Silero VAD V5) is shared across engines — it's a
/// speech-trained neural model that outperforms the old RMS threshold on
/// music, keyboard clicks, HVAC noise, and quiet speech. Only the
/// *timing* knobs are engine-specific:
///
/// - Whisper: the original dictation-tuned cadence (honors the user's
///   frontend `silence_duration_ms`, 300 ms poll, no pre-roll). Values
///   preserve the proven Whisper dictation feel — Silero only changes
///   *what* we detect as speech, not *how long* we wait before chunking.
/// - Parakeet: meeting-tuned cadence (500 ms silence, 10 s max
///   chunk, 100 ms poll, 250 ms pre-roll). Parakeet's low RTFx and
///   non-autoregressive decoder make short chunks free; we bias for
///   responsive on-screen updates.
///
/// `speech_threshold` / `offset_threshold_ratio` are now Silero
/// *probability* thresholds (V5 defaults: 0.5 / 0.35) rather than RMS
/// energy thresholds. They're held per-tuning so a future engine could
/// override them if needed, but today both engines use the same values.
#[derive(Debug, Clone, Copy)]
struct VadTuning {
    /// Speech-probability threshold at or above which a frame is
    /// considered speech (Silero V5 default 0.5).
    speech_threshold: f32,
    /// End-of-speech threshold as a ratio of `speech_threshold`. Must be
    /// in (0.0, 1.0]. 0.7 yields the V5-default 0.35 end threshold
    /// (hysteresis — prevents flapping on short probability dips).
    offset_threshold_ratio: f32,
    /// How long silence must hold before chunking.
    silence_duration: Duration,
    /// Force-chunk after this much continuous speech.
    max_chunk_duration: Duration,
    /// Inner poll cadence for VAD probability checks + transitions.
    poll_interval: Duration,
    /// Pre-onset capture: extraction starts this far *before* the point we
    /// noticed energy cross the threshold, to catch leading plosives.
    pre_roll: Duration,
}

/// Engine-conditional configuration consolidated into a single value
/// resolved once at session start. The live loop and `transcribe_chunk`
/// consult this instead of re-deriving from `EngineKind` at every site —
/// keeps the path clean of `match engine_kind` sprawl as the upcoming
/// queue / watchdog / repair stages add more engine-aware behavior.
#[derive(Debug, Clone)]
struct EngineProfile {
    /// Underlying engine identity. Kept here so call sites that need the
    /// raw enum (hallucination filtering, IPC requests) don't have to pass
    /// it separately alongside the profile.
    engine_kind: yapstack_common::types::EngineKind,
    /// Display name for telemetry surfaces — `"Whisper"` / `"Parakeet"`.
    engine_name: &'static str,
    /// Per-source VAD timing + thresholds.
    vad_tuning: VadTuning,
    /// Whisper's whisper-rs decoder consumes `initial_prompt` (vocabulary
    /// hints + rolling accumulated_text); Parakeet TDT's decoder ignores
    /// any text prompt. Drives prompt-context build / decay sites so
    /// neither engine has to be matched on directly.
    uses_initial_prompt: bool,
    /// Resolved acceleration label captured from the live
    /// `TranscriptionClient`'s engine_info at session start. Populated
    /// when the FE knows what's running; `None` for older sidecars or
    /// when the spawn-time query failed (`init_transcription_client`
    /// already logs that case).
    accel: Option<String>,
    /// For Parakeet, the variant directory name (e.g.
    /// `"parakeet-tdt-v3-int8"`). Lets the pressure-event log tell
    /// int8 from fp32 sessions without external joins.
    variant: Option<String>,
}

fn profile_for(
    engine: yapstack_common::types::EngineKind,
    config: &LiveTranscriptionConfig,
) -> EngineProfile {
    use yapstack_common::types::EngineKind;
    match engine {
        // Whisper: preserve existing dictation-proven *timing* exactly.
        // Silence window honors the user's `silence_duration_ms` (frontend
        // default 800 ms); 300 ms poll cadence; no pre-roll. Only the
        // detector swaps RMS → Silero — all timing constants stay put.
        EngineKind::Whisper => EngineProfile {
            engine_kind: engine,
            engine_name: "Whisper",
            vad_tuning: VadTuning {
                speech_threshold: SPEECH_THRESHOLD,
                offset_threshold_ratio: SILENCE_THRESHOLD / SPEECH_THRESHOLD,
                silence_duration: Duration::from_millis(config.silence_duration_ms as u64),
                max_chunk_duration: Duration::from_secs_f32(config.max_chunk_seconds),
                poll_interval: Duration::from_millis(POLL_INTERVAL_MS),
                pre_roll: Duration::ZERO,
            },
            uses_initial_prompt: true,
            accel: None,
            variant: None,
        },
        // Parakeet: meeting-tuned. Ignores frontend silence / chunk / poll
        // knobs — these are engine-specific best practice, not user-facing
        // tuning.
        //
        // `silence_duration` was 200 ms originally, which mirrors a
        // dictation-style "fire as soon as the speaker pauses" cadence.
        // For multi-speaker meeting transcription that produces ~3× more
        // dispatches than necessary — every comma-breath triggers a chunk —
        // and the IPC + inference pressure of those extra dispatches is the
        // proximate cause of the stall the cap commit (e6b05ea) was trying
        // to bound. Comparable production stacks use 500 ms (Vad+Whisper
        // dictation), 750 ms (FluidAudio meeting mode), or higher (Muesli
        // aims for 3 s minimum chunks). Bumped to 500 ms — the conservative
        // half of that range — to cut dispatch rate by ~2-3× on fast
        // multi-speaker dialogue while keeping live-caption feel.
        //
        // `max_chunk_duration` stays at 10 s for now: the cap commit
        // documented chunks ≥12 s degrading to RTFx < 1 on our CPU path.
        // FluidAudio's 14.4 s window is feasible because they run on the
        // ANE; we don't have that path until we either (a) get TDT v3
        // working on ORT-CoreML or (b) switch to streaming Nemotron.
        // Stage 6 of the pipeline overhaul plan revisits once we have
        // real per-chunk wall-time data to validate.
        EngineKind::Parakeet => EngineProfile {
            engine_kind: engine,
            engine_name: "Parakeet",
            vad_tuning: VadTuning {
                speech_threshold: SPEECH_THRESHOLD,
                offset_threshold_ratio: SILENCE_THRESHOLD / SPEECH_THRESHOLD,
                silence_duration: Duration::from_millis(500),
                max_chunk_duration: Duration::from_secs(10),
                poll_interval: Duration::from_millis(100),
                pre_roll: Duration::from_millis(250),
            },
            uses_initial_prompt: false,
            accel: None,
            variant: None,
        },
    }
}

/// Polls the VAD state machine for a single source. The `probability`
/// input is Silero's per-frame speech probability in [0, 1]; `None`
/// means no full frame accumulated during this poll window (in which
/// case the state machine stays put — no toggles on missing data).
/// Manages `speech_start_time` internally on state transitions.
/// Uses `tuning.speech_threshold` for onset detection and
/// `tuning.speech_threshold * tuning.offset_threshold_ratio` for offset
/// (hysteresis prevents mid-word dropout on short probability dips).
fn poll_vad(state: &mut SourceVadState, probability: Option<f32>, tuning: &VadTuning) -> VadAction {
    let Some(probability) = probability else {
        return VadAction::None;
    };

    if state.is_speaking {
        let offset_threshold = tuning.speech_threshold * tuning.offset_threshold_ratio;
        let is_loud = probability >= offset_threshold;
        if is_loud {
            state.silence_since = None;

            let speech_elapsed = state
                .speech_start_time
                .map(|t| t.elapsed())
                .unwrap_or_default();
            if speech_elapsed >= tuning.max_chunk_duration {
                return VadAction::ForceChunk;
            }

            VadAction::None
        } else {
            let silence_start = *state.silence_since.get_or_insert_with(Instant::now);
            if silence_start.elapsed() >= tuning.silence_duration {
                VadAction::Chunk
            } else {
                VadAction::None
            }
        }
    } else {
        let is_loud = probability >= tuning.speech_threshold;
        if is_loud {
            state.is_speaking = true;
            state.silence_since = None;
            state.speech_start_time = Some(Instant::now());
            // Pre-roll: rewind speech_start_pos so the extracted chunk
            // includes the audio immediately *before* we noticed energy
            // cross the threshold. Clamped to `earliest_next_chunk_pos`
            // (last dispatched chunk's end, or session_start_pos when no
            // chunk has been dispatched yet) so we never re-transcribe the
            // tail of a previous chunk when onset happens soon after a
            // dispatch — e.g. a second utterance while the previous
            // chunk's task is still in flight.
            if tuning.pre_roll > Duration::ZERO {
                let pre_roll_samples = (tuning.pre_roll.as_secs_f32()
                    * state.source_sample_rate as f32
                    * state.source_channels as f32) as usize;
                state.speech_start_pos = state
                    .speech_start_pos
                    .saturating_sub(pre_roll_samples)
                    .max(state.earliest_next_chunk_pos);
            }
        }
        VadAction::None
    }
}

// --- Helpers ---

/// Per-event progress snapshot bundled into `LiveTranscriptionStatus`.
/// These five fields always travel together at every emit site; grouping
/// them keeps `emit_status` to a manageable signature and lets the two
/// "no data yet" sites use `StatusProgress::zero()` instead of repeating
/// the literals.
struct StatusProgress {
    chunks: u32,
    audio_secs: f32,
    lag_seconds: Option<f32>,
    live_drain_backlog_chunks: u32,
    live_drain_backlog_seconds: f32,
}

impl StatusProgress {
    fn zero() -> Self {
        Self {
            chunks: 0,
            audio_secs: 0.0,
            lag_seconds: None,
            live_drain_backlog_chunks: 0,
            live_drain_backlog_seconds: 0.0,
        }
    }
}

fn emit_status(
    ctx: &TranscriptionContext,
    phase: LiveTranscriptionPhase,
    progress: StatusProgress,
) {
    let _ = ctx.app_handle.emit(
        "live-transcription-status",
        LiveTranscriptionStatus {
            phase,
            chunks_processed: progress.chunks,
            total_audio_seconds: progress.audio_secs,
            error_message: None,
            // Carrying session_id and source_kind on every status event is
            // load-bearing for the frontend's two-prong filter: dictation
            // listeners only resolve their stop-promise on a status whose
            // session_id matches their synthetic dictation id, and the
            // session UI only flips its phase machine on its own session's
            // events. Without this, a dictation Stopped emit looked like a
            // session Stopped emit (None == None, source_kind=session by
            // default), so the dictation hook never observed completion
            // and waited the full 5s timeout before AI / output / history.
            session_id: ctx.config.session_id.clone(),
            effective_start_epoch_ms: None,
            lag_seconds: progress.lag_seconds,
            live_drain_backlog_chunks: progress.live_drain_backlog_chunks,
            live_drain_backlog_seconds: progress.live_drain_backlog_seconds,
            source_kind: ctx.config.source_kind,
        },
    );
}

/// Zero out mono-frame samples in a session-WAV extraction that fall
/// inside any dictation window — open or closed. Without this, the
/// recorded WAV file contains the dictated message even though the
/// transcript correctly skipped it, creating a confusing playback /
/// transcript mismatch.
///
/// The extracted samples are mono (deinterleaved). One mono frame
/// covers `mic_channels` raw mic-buffer positions, so a sample at
/// mono index `i` corresponds to raw range
/// `[old_pos + i*ch, old_pos + (i+1)*ch)`. We compute the mono index
/// span for each overlapping window and zero only those samples.
///
/// Source semantics:
/// - `MicOnly`: extraction is mic mono; precise zeroing → silence
///   inside the dictation window, untouched session audio outside it.
/// - `Mixed`: extraction is the (mic + system) mix at mic rate; we
///   still compute the overlap against mic positions and zero those
///   samples. The mute loses the system-audio portion during the
///   dictation window; a fully precise implementation would extract
///   system-only audio for that range and substitute it. Documented
///   here as intentional MVP behavior — better than recording the
///   dictation in the session WAV.
/// - `SystemOnly`: never enters this helper — dictation can't affect
///   the system buffer.
///
/// `old_pos` is the mic write position the extraction started from
/// (i.e. the WAV writer's pre-extraction `flush_positions.mic_pos`).
/// Without it we can't map mono indices back to ring-buffer positions
/// for window-overlap math.
fn mute_dictation_window_in_wav_extraction(
    samples: &mut [f32],
    mic_channels: usize,
    old_mic_pos: u64,
    ctx: &TranscriptionContext,
    wav_source: CaptureSource,
) {
    if !matches!(ctx.config.source_kind, LiveSourceKind::Session) {
        return;
    }
    if !matches!(wav_source, CaptureSource::MicOnly | CaptureSource::Mixed) {
        return;
    }
    if mic_channels == 0 || samples.is_empty() {
        return;
    }
    let snapshot = ctx.dictation_owns_mic.snapshot();
    let frame_count = samples.len() as u64;
    let extract_end_pos = old_mic_pos + frame_count * (mic_channels as u64);

    // Iterate windows (closed in chronological order, then the virtual
    // open window) and zero each overlap.
    for window in snapshot.iter_all() {
        if window.end <= old_mic_pos || window.start >= extract_end_pos {
            continue;
        }
        let overlap_start = window.start.max(old_mic_pos);
        let overlap_end = window.end.min(extract_end_pos);
        // Convert raw positions to mono frame indices, rounding the
        // start *down* and the end *up* so a partial frame at either
        // boundary is fully muted (better to lose a frame of session
        // audio than leak a frame of dictation).
        let start_frame = ((overlap_start - old_mic_pos) / (mic_channels as u64)) as usize;
        let end_frame = (overlap_end - old_mic_pos).div_ceil(mic_channels as u64) as usize;
        let end_frame = end_frame.min(samples.len());
        if start_frame < end_frame {
            for s in &mut samples[start_frame..end_frame] {
                *s = 0.0;
            }
        }
    }
}

fn update_scheduler_live_busy(
    ctx: &TranscriptionContext,
    sources: &[SourceVadState],
    in_flight_tasks: usize,
    stopping: bool,
) {
    let busy = stopping
        || in_flight_tasks > 0
        || sources
            .iter()
            .any(|s| s.is_speaking || s.has_in_flight_task || s.force_drain);
    ctx.config
        .source_kind
        .set_scheduler_busy(&ctx.scheduler, busy);
}

fn record_live_drain_backlog(
    counters: &Arc<StdMutex<SessionCounters>>,
    origin: JobOrigin,
    backlog_seconds: f32,
) {
    if !matches!(origin, JobOrigin::Live) {
        return;
    }
    let mut s = counters.lock().expect("counters mutex poisoned");
    s.live_drain_backlog_seconds = backlog_seconds;
    if backlog_seconds > 0.0 {
        s.live_drain_backlog_chunks = s.live_drain_backlog_chunks.saturating_add(1);
    }
}

async fn receive_stop_request_or_snapshot(
    stop_rx: &mut oneshot::Receiver<StopRequest>,
    audio_state: &AudioManagerState,
) -> StopRequest {
    match stop_rx.await {
        Ok(req) => req,
        Err(_) => {
            let manager = audio_state.lock().await;
            StopRequest {
                positions: stop_positions_with_tail(&manager),
            }
        }
    }
}

async fn stop_if_requested(
    stop_requested: &AtomicBool,
    stop_rx: &mut oneshot::Receiver<StopRequest>,
    audio_state: &AudioManagerState,
    ctx: &TranscriptionContext,
) -> Option<StopRequest> {
    if !stop_requested.load(Ordering::Acquire) {
        return None;
    }
    // Defensively block backfill from racing into the lane between stop
    // observation and final-flush dispatch.
    ctx.config
        .source_kind
        .set_scheduler_busy(&ctx.scheduler, true);
    Some(receive_stop_request_or_snapshot(stop_rx, audio_state).await)
}

fn tail_samples_for(sample_rate: u32, channels: u16) -> usize {
    let channels = channels.max(1) as usize;
    let raw = (STOP_TAIL_GRACE_SECS * sample_rate as f32 * channels as f32) as usize;
    raw - (raw % channels)
}

fn stop_positions_with_tail(manager: &AudioManager) -> BufferPositions {
    let positions = manager.buffer_positions();
    let mic_tail = manager
        .mic_buffer_info()
        .map(|info| tail_samples_for(info.sample_rate, info.channels))
        .unwrap_or(0);
    let system_tail = manager
        .system_buffer_info()
        .map(|info| tail_samples_for(info.sample_rate, info.channels))
        .unwrap_or(0);
    BufferPositions {
        mic_pos: positions.mic_pos.saturating_add(mic_tail),
        system_pos: positions.system_pos.saturating_add(system_tail),
    }
}

/// Get the current write position for a source from the manager.
fn source_write_pos(manager: &AudioManager, label: &AudioSourceLabel) -> usize {
    match label {
        AudioSourceLabel::Mic => manager.mic_write_pos(),
        AudioSourceLabel::System => manager.system_write_pos(),
    }
}

/// Extract mono audio from a source's ring buffer starting at `from_pos`.
///
/// Returns `(audio, new_pos)` where `new_pos` is the atomic write position
/// captured at the same instant as the snapshot. This eliminates the race
/// window where cpal callbacks advance write_pos between snapshot and
/// position query.
fn extract_source_audio(
    manager: &AudioManager,
    label: &AudioSourceLabel,
    from_pos: usize,
) -> (Option<(Vec<f32>, u32)>, usize) {
    let buf = match label {
        AudioSourceLabel::Mic => manager.mic_buffer(),
        AudioSourceLabel::System => manager.system_buffer(),
    };
    let Some(buf) = buf else {
        return (None, from_pos);
    };
    let (snap, new_pos) = buf.snapshot_since_with_pos(from_pos);
    if snap.is_empty() {
        return (None, new_pos);
    }
    let mono = yapstack_common::audio::deinterleave_to_mono(&snap, buf.channels()).into_owned();
    (Some((mono, buf.sample_rate())), new_pos)
}

struct SourceAudioRange {
    samples: Vec<f32>,
    sample_rate: u32,
    start_pos: usize,
    end_pos: usize,
    overrun: bool,
}

/// Extract mono audio from a source's ring buffer in `[from_pos, until_pos)`.
/// Used for hard-stop and queue-drain paths that must not read to the live
/// write head implicitly.
fn extract_source_audio_until(
    manager: &AudioManager,
    label: &AudioSourceLabel,
    from_pos: usize,
    until_pos: usize,
) -> Option<SourceAudioRange> {
    let buf = match label {
        AudioSourceLabel::Mic => manager.mic_buffer(),
        AudioSourceLabel::System => manager.system_buffer(),
    }?;
    let snap = buf.snapshot_range(from_pos, until_pos);
    if snap.samples.is_empty() {
        return None;
    }
    let mono =
        yapstack_common::audio::deinterleave_to_mono(&snap.samples, buf.channels()).into_owned();
    if mono.is_empty() {
        return None;
    }
    Some(SourceAudioRange {
        samples: mono,
        sample_rate: buf.sample_rate(),
        start_pos: snap.start_pos,
        end_pos: snap.end_pos,
        overrun: snap.overrun,
    })
}

fn max_raw_samples_for_duration(duration: Duration, sample_rate: u32, channels: u16) -> usize {
    let ch = channels.max(1) as usize;
    let raw = (duration.as_secs_f32() * sample_rate as f32 * ch as f32) as usize;
    raw.max(ch) - (raw.max(ch) % ch)
}

/// Boxed future that yields a `ChunkTaskOutcome` once a spawned chunk
/// transcribe finishes (or its panic-recovery handler synthesizes one).
/// Held by `live_transcription_loop`'s `FuturesUnordered` and drained by the
/// post-loop helpers.
type ChunkTaskFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = ChunkTaskOutcome> + Send>>;

/// Outcome returned from a background chunk task back to the main loop so it
/// can restore per-source state (accumulated_text) and notice fatal failures.
struct ChunkTaskOutcome {
    source_label: AudioSourceLabel,
    /// Returned so the main loop can write it back to `vad.accumulated_text`.
    /// The task mutates a moved-in copy; moving back on completion is safe
    /// because we gate VAD polling on `has_in_flight_task` — no concurrent
    /// reader of `vad.accumulated_text` exists while the task runs.
    accumulated_text: String,
    /// True when the sidecar died and could not be restarted. Main loop
    /// exits on receiving this.
    sidecar_dead: bool,
}

/// Prepare a chunk for background transcription: extract audio, advance the
/// source's VAD cursor/state optimistically (as if the task will succeed),
/// and return the parameters needed to spawn the task. Returns `None` when
/// there's no audio to transcribe yet (too short / empty extraction) so the
/// caller skips task spawn and the next poll re-tries.
///
/// Sets `vad.has_in_flight_task = true` when a task is dispatched. The main
/// loop clears it when the task's outcome lands.
async fn prepare_chunk_dispatch(
    vad: &mut SourceVadState,
    audio_state: &AudioManagerState,
    is_force_chunk: bool,
    origin: JobOrigin,
    max_chunk_duration: Duration,
) -> Option<PreparedChunk> {
    prepare_chunk_dispatch_inner(
        vad,
        audio_state,
        is_force_chunk,
        origin,
        max_chunk_duration,
        None,
    )
    .await
}

/// Spawn a chunk task for the session mic source's pending speech bounded
/// at `ceiling_pos` — used by the window pre-flush logic to flush the
/// user's word-in-progress up to a dictation window's `start` before
/// resetting state past `end`. Wraps the boilerplate that the inline
/// rising-edge handler used to repeat: prepare the chunk, record drain
/// backlog, spawn the task, push a panic-tolerant join handle into
/// `chunk_tasks`. Caller has already verified `is_speaking == true`,
/// `has_in_flight_task == false`, and `ceiling_pos > speech_start_pos`.
///
/// The argument list is long because every parameter is a distinct
/// piece of live-loop state the helper needs &mut access to. Bundling
/// into a struct adds construction boilerplate at the two call sites
/// without simplifying anything; the lint is a smell-detector that
/// doesn't apply to internal "pull this duplicated block into one
/// place" helpers like this one.
#[allow(clippy::too_many_arguments)]
async fn dispatch_pre_window_flush(
    vad: &mut SourceVadState,
    audio_state: &AudioManagerState,
    ctx: &TranscriptionContext,
    counters: &Arc<StdMutex<SessionCounters>>,
    prompt: &Arc<StdMutex<PromptState>>,
    chunk_tasks: &mut futures_util::stream::FuturesUnordered<ChunkTaskFuture>,
    max_chunk_duration: Duration,
    ceiling_pos: usize,
) {
    let prepared = prepare_chunk_dispatch_until(
        vad,
        audio_state,
        true,
        JobOrigin::Live,
        max_chunk_duration,
        ceiling_pos,
    )
    .await;
    let Some(prepared) = prepared else { return };
    record_live_drain_backlog(counters, prepared.origin, prepared.drain_backlog_seconds);
    let task_ctx = ctx.clone();
    let task_counters = counters.clone();
    let task_prompt = prompt.clone();
    let source_label = prepared.source_label;
    let fallback_text = prepared.accumulated_text.clone();
    let handle = tokio::spawn(async move {
        run_chunk_task(prepared, task_ctx, task_counters, task_prompt).await
    });
    chunk_tasks.push(Box::pin(async move {
        match handle.await {
            Ok(outcome) => outcome,
            Err(e) => {
                error!(
                    "dictation pre-window flush task panicked or was cancelled: {} \
                     — synthesizing outcome to free dispatch",
                    e
                );
                ChunkTaskOutcome {
                    source_label,
                    accumulated_text: fallback_text,
                    sidecar_dead: false,
                }
            }
        }
    }));
}

/// Variant that bounds the dispatch upper end at `ceiling_pos` instead of
/// the current write position. Used by the dictation rising-edge flush —
/// when dictation acquires the mic mid-session-utterance, we want to
/// dispatch the session's pending speech only up to the boundary
/// `acquired_at`, not up to whatever write position has accumulated since
/// (that audio is dictation content).
async fn prepare_chunk_dispatch_until(
    vad: &mut SourceVadState,
    audio_state: &AudioManagerState,
    is_force_chunk: bool,
    origin: JobOrigin,
    max_chunk_duration: Duration,
    ceiling_pos: usize,
) -> Option<PreparedChunk> {
    prepare_chunk_dispatch_inner(
        vad,
        audio_state,
        is_force_chunk,
        origin,
        max_chunk_duration,
        Some(ceiling_pos),
    )
    .await
}

async fn prepare_chunk_dispatch_inner(
    vad: &mut SourceVadState,
    audio_state: &AudioManagerState,
    is_force_chunk: bool,
    origin: JobOrigin,
    max_chunk_duration: Duration,
    ceiling_pos: Option<usize>,
) -> Option<PreparedChunk> {
    let max_raw_samples = max_raw_samples_for_duration(
        max_chunk_duration,
        vad.source_sample_rate,
        vad.source_channels,
    );
    let current_pos = match ceiling_pos {
        Some(p) => p,
        None => {
            let manager = audio_state.lock().await;
            source_write_pos(&manager, &vad.label)
        }
    };
    let dispatch_until = current_pos.min(vad.speech_start_pos.saturating_add(max_raw_samples));

    let extraction = {
        let manager = audio_state.lock().await;
        extract_source_audio_until(&manager, &vad.label, vad.speech_start_pos, dispatch_until)
    };

    let Some(extraction) = extraction else {
        // Nothing new in the buffer since last read — advance the cursor to
        // the latest write_pos but leave speech_start_pos so we'll pick up
        // the ongoing utterance on the next poll.
        vad.cursor = dispatch_until;
        return None;
    };
    if extraction.overrun {
        warn!(
            marker = "live_ring_overrun",
            source = ?vad.label,
            requested_start = vad.speech_start_pos,
            actual_start = extraction.start_pos,
            end = extraction.end_pos,
            "ring buffer overrun while preparing live chunk; oldest unavailable samples were skipped"
        );
    }

    let extracted_duration = extraction.samples.len() as f32 / extraction.sample_rate as f32;
    if extracted_duration < MIN_CHUNK_DURATION_SECS {
        // Too short — don't dispatch yet and don't advance speech_start_pos.
        // Next poll re-extracts this region together with whatever arrives.
        vad.cursor = extraction.end_pos;
        return None;
    }

    let chunk_duration = extraction.samples.len() as f32 / extraction.sample_rate as f32;
    let has_remaining = current_pos > extraction.end_pos;
    let drain_backlog_seconds = current_pos.saturating_sub(extraction.end_pos) as f32
        / (vad.source_sample_rate as f32 * vad.source_channels as f32);

    // Deterministic offset from buffer position delta. The anchor encodes
    // "session-time at which this source's `session_start_pos` was set" —
    // at loop entry it equals `ctx.session_offset_base_seconds` (so persisted
    // Segments stay continuous past prior session parts on resume), and on
    // every `BufferReplaced` it advances by the wall-clock time elapsed
    // inside the current loop, keeping post-swap chunks aligned with real
    // session-time instead of silently rewinding by minutes.
    let samples_since_start = extraction.start_pos.saturating_sub(vad.session_start_pos);
    let audio_offset = vad.audio_offset_anchor_seconds
        + samples_since_start as f32 / (vad.source_sample_rate as f32 * vad.source_channels as f32);

    debug!(
        "live chunk: source={:?} offset={:.2}s duration={:.2}s samples={} (pos: start={} end={} session_start={}, remaining={})",
        vad.label,
        audio_offset,
        chunk_duration,
        extraction.samples.len(),
        extraction.start_pos,
        extraction.end_pos,
        vad.session_start_pos,
        has_remaining,
    );

    // Snapshot chunk_index for this task, increment our counter so the next
    // chunk (if any) gets a unique id. The task emits events carrying this
    // index; we don't need to write back.
    let chunk_index = vad.chunk_index;
    vad.chunk_index = vad.chunk_index.wrapping_add(1);

    // Move accumulated_text into the task; the main loop restores it on
    // outcome. Safe because VAD polling is gated off this source until then.
    let accumulated_text = std::mem::take(&mut vad.accumulated_text);

    // Optimistically advance cursor + VAD state as if the task will succeed.
    // On transient failures we quarantine the audio rather than retry the
    // same region — the main loop continues unblocked, and the user gets a
    // visible warning with a recoverable WAV path.
    vad.cursor = extraction.end_pos;
    vad.is_speaking = has_remaining || is_force_chunk;
    vad.silence_since = None;
    vad.speech_start_pos = extraction.end_pos;
    // Pre-roll for any subsequent onset must not pull speech_start_pos
    // before this point, or the next chunk would redundantly transcribe
    // the tail of the audio this task is about to send.
    vad.earliest_next_chunk_pos = extraction.end_pos;
    vad.force_drain = has_remaining;
    if is_force_chunk || has_remaining {
        vad.speech_start_time = Some(Instant::now());
    } else {
        vad.speech_start_time = None;
    }

    vad.has_in_flight_task = true;

    Some(PreparedChunk {
        samples: extraction.samples,
        sample_rate: extraction.sample_rate,
        audio_offset_seconds: audio_offset,
        source_label: vad.label,
        chunk_index,
        accumulated_text,
        origin,
        drain_backlog_seconds,
    })
}

/// Owned chunk data handed off to a spawned background task. Contains
/// everything the task needs; no references into the main-loop state.
struct PreparedChunk {
    samples: Vec<f32>,
    sample_rate: u32,
    audio_offset_seconds: f32,
    source_label: AudioSourceLabel,
    chunk_index: u32,
    accumulated_text: String,
    origin: JobOrigin,
    drain_backlog_seconds: f32,
}

/// Run a single chunk's transcription in a background task. Emits the
/// segment event on success; quarantines the audio and emits a warning on
/// failure. Returns the outcome so the main loop can restore
/// `accumulated_text` and notice a dead sidecar.
async fn run_chunk_task(
    prepared: PreparedChunk,
    ctx: TranscriptionContext,
    counters: Arc<StdMutex<SessionCounters>>,
    prompt: Arc<StdMutex<PromptState>>,
) -> ChunkTaskOutcome {
    let source_label = prepared.source_label;
    let chunk_index_for_quarantine = prepared.chunk_index;
    let samples_snapshot = prepared.samples.clone();
    let sample_rate_snapshot = prepared.sample_rate;

    let input = ChunkInput {
        samples: &prepared.samples,
        sample_rate: prepared.sample_rate,
        audio_offset_seconds: prepared.audio_offset_seconds,
        source_label,
        drain_backlog_seconds: prepared.drain_backlog_seconds,
        origin: prepared.origin,
    };
    let mut chunk_index_mut = prepared.chunk_index;
    let mut accumulated_text = prepared.accumulated_text;

    let outcome = transcribe_and_emit_chunk(
        &ctx,
        &input,
        &mut chunk_index_mut,
        &mut accumulated_text,
        &counters,
        &prompt,
    )
    .await;

    let sidecar_dead = matches!(outcome, TranscribeOutcome::SidecarDead);
    let skipped = matches!(outcome, TranscribeOutcome::Skipped);

    if skipped {
        // Transient failure (temp WAV write error, engine error, timeout).
        // Quarantine the audio immediately so the user can recover it, and
        // emit a visible warning. No retry — retry-in-place doesn't help
        // when the failure is persistent, and concurrent dispatch means the
        // main loop isn't blocked waiting to retry.
        let duration_s = samples_snapshot.len() as f32 / sample_rate_snapshot as f32;
        let quarantine_path = quarantine_chunk_to_wav(
            &ctx.app_handle,
            ctx.config.session_id.as_deref(),
            &source_label,
            chunk_index_for_quarantine,
            &samples_snapshot,
            sample_rate_snapshot,
        );
        let path_str = quarantine_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<no audio captured>".to_string());
        warn!(
            "live transcription: chunk failed for source {:?} — quarantined {:.2}s to {}",
            source_label, duration_s, path_str
        );
        let _ = ctx.app_handle.emit(
            "live-transcription-warning",
            LiveTranscriptionWarningEvent {
                message: format!(
                    "{:.1}s of audio could not be transcribed; saved for debugging at {}",
                    duration_s, path_str
                ),
            },
        );
    }

    ChunkTaskOutcome {
        source_label,
        accumulated_text,
        sidecar_dead,
    }
}

/// Save an abandoned chunk's audio to the quarantine directory so the user/dev
/// can inspect what was lost when retries exhausted. Returns the file path on
/// success; logs (without the `?` operator) and returns `None` on any failure
/// — quarantine is best-effort and must never break the live loop.
fn quarantine_chunk_to_wav(
    app: &AppHandle,
    session_id: Option<&str>,
    label: &AudioSourceLabel,
    chunk_index: u32,
    samples: &[f32],
    sample_rate: u32,
) -> Option<std::path::PathBuf> {
    let base = match app.path().app_data_dir() {
        Ok(d) => d.join("audio").join("quarantine"),
        Err(e) => {
            warn!("quarantine: cannot resolve app_data_dir: {}", e);
            return None;
        }
    };
    let dir = match session_id {
        Some(sid) => base.join(sid),
        None => base.join("dictation"),
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!("quarantine: cannot create dir {}: {}", dir.display(), e);
        return None;
    }
    let source_str = match label {
        AudioSourceLabel::Mic => "mic",
        AudioSourceLabel::System => "system",
    };
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ");
    let path = dir.join(format!("{ts}_{source_str}_{chunk_index}.wav"));
    if let Err(e) = yapstack_audio::export::write_wav(samples, sample_rate, 1, &path) {
        warn!("quarantine: write_wav failed at {}: {}", path.display(), e);
        return None;
    }
    Some(path)
}

/// Advance the idle cursor to the current write position when not speaking.
async fn advance_idle_cursor(vad: &mut SourceVadState, audio_state: &AudioManagerState) {
    let new_pos = {
        let manager = audio_state.lock().await;
        source_write_pos(&manager, &vad.label)
    };
    vad.cursor = new_pos;
    vad.speech_start_pos = new_pos;
}

// --- Prompt decay ---

/// Clear prompt context when no successful transcription has occurred within
/// `decay_seconds`. This prevents stale text from causing hallucinations
/// (e.g. saying "Hello", waiting 10s with ambient noise, then Whisper
/// hallucinating "Hello" from noise because the old prompt is still primed).
///
/// Uses time-since-last-transcription rather than VAD silence detection,
/// because ambient noise (keyboard, AC, fan) easily exceeds the RMS threshold
/// and keeps resetting the silence timer, preventing decay from ever firing.
/// Noise-only chunks produce `TranscribeOutcome::Skipped` and do NOT update
/// `last_transcription_at`, so intermittent noise cannot prevent decay.
///
/// Returns `true` if any prompt state was cleared.
fn check_prompt_decay(
    sources: &mut [SourceVadState],
    shared_prompt: &mut String,
    decay_seconds: f32,
    last_transcription_at: Option<Instant>,
) -> bool {
    if decay_seconds <= 0.0 {
        return false;
    }
    let Some(last_t) = last_transcription_at else {
        return false;
    };
    if last_t.elapsed() < Duration::from_secs_f32(decay_seconds) {
        return false;
    }
    let has_prompt_state =
        !shared_prompt.is_empty() || sources.iter().any(|s| !s.accumulated_text.is_empty());
    if !has_prompt_state {
        return false;
    }
    shared_prompt.clear();
    for source in sources.iter_mut() {
        source.accumulated_text.clear();
    }
    true
}

// --- Background loop ---

/// Internal poll interval for the VAD loop.
const POLL_INTERVAL_MS: u64 = 300;
/// Minimum chunk duration in seconds below which a chunk is skipped.
const MIN_CHUNK_DURATION_SECS: f32 = 0.1;
/// Number of consecutive empty WAV flush extractions before emitting an error event.
const WAV_FLUSH_ERROR_THRESHOLD: u32 = 10;
/// Interval (in consecutive empty flushes) for emitting periodic warnings.
const WAV_FLUSH_WARNING_INTERVAL: u32 = 20;
/// Interval (in successful flushes) for emitting periodic diagnostic logs.
const WAV_FLUSH_DIAGNOSTIC_INTERVAL: u32 = 100;
/// Maximum chars of accumulated transcription text kept per source.
const MAX_ACCUMULATED_TEXT_CHARS: usize = 1000;
/// Seconds of write_pos stall before triggering a stream restart.
const STREAM_STALL_THRESHOLD_SECS: f32 = 2.0;
/// Maximum auto-restart attempts per source before giving up.
const STREAM_RESTART_MAX_ATTEMPTS: u32 = 3;
/// Minimum seconds between restart attempts for the same source.
const STREAM_RESTART_COOLDOWN_SECS: f32 = 5.0;
/// Maximum audio duration for one backfill job submitted to the scheduler.
/// Sidecar work is not preemptible once started, so keeping historical jobs
/// short bounds how long newly-arrived live speech can wait behind one.
const BACKFILL_JOB_QUANTUM_SECS: f32 = 5.0;
/// Small bounded grace period included after the stop command snapshots the
/// ring-buffer positions. This catches the final syllable already in the
/// OS/audio callback pipeline without returning to unbounded post-stop reads.
const STOP_TAIL_GRACE_SECS: f32 = 0.3;

// --- Pure stream health decision helpers ---

/// Returns `true` if a write-position stall should trigger a stream restart for the
/// given source. On Windows, system audio loopback produces zero samples when nothing
/// is playing — this is normal WASAPI behavior, not a stream failure.
fn should_stall_restart(label: &AudioSourceLabel) -> bool {
    !(cfg!(target_os = "windows") && matches!(label, AudioSourceLabel::System))
}

/// Window used by the pre-flight health check to observe whether `write_pos`
/// is advancing. Must be long enough to see at least one cpal callback
/// (typical buffer periods are 5–20 ms) and short enough to not add noticeable
/// latency to the hotkey → dictation-start flow.
const PREFLIGHT_SAMPLE_WINDOW_MS: u64 = 80;

/// Pre-flight stream health check. Run before the live-transcription loop
/// starts to catch silently-stalled cpal streams — device changes, Bluetooth
/// disconnects, OS wake-from-sleep — that don't raise an error callback. The
/// in-loop watchdog only fires after `STREAM_STALL_THRESHOLD_SECS` (2 s), by
/// which point the first transcribed chunk has already been extracted from
/// stale buffer data, producing wildly inaccurate or empty transcriptions
/// after long idle periods.
///
/// Strategy: snapshot each relevant source's `write_pos`, wait briefly, check
/// again. If any source has its error flag set or failed to advance, restart
/// it before spawning the live loop. A failed restart on a user-requested
/// source propagates as an error; a failed system-audio restart in mixed mode
/// is logged but allowed to proceed (the loop will degrade to mic-only).
async fn preflight_stream_health(
    audio_state: &AudioManagerState,
    source: &CaptureSource,
    app_handle: &AppHandle,
) -> Result<(), CommandError> {
    let check_mic = matches!(source, CaptureSource::MicOnly | CaptureSource::Mixed);
    let check_system = matches!(source, CaptureSource::SystemOnly | CaptureSource::Mixed);

    if !check_mic && !check_system {
        return Ok(());
    }

    // Snapshot symptom-layer state + consume any pending default-device-
    // change notification. Consuming the flag here means a listener event
    // fired between sessions still triggers a rebind on preflight, before
    // first extraction reads stale audio.
    let (mic_initial, sys_initial, mic_err, sys_err, has_mic_buf, has_sys_buf) = {
        let m = audio_state.lock().await;
        (
            m.mic_write_pos(),
            m.system_write_pos(),
            m.mic_has_stream_error(),
            m.system_has_stream_error(),
            m.mic_buffer().is_some(),
            m.system_buffer().is_some(),
        )
    };

    let check_mic = check_mic && has_mic_buf;
    let check_system = check_system && has_sys_buf;

    if !check_mic && !check_system {
        return Ok(());
    }

    tokio::time::sleep(Duration::from_millis(PREFLIGHT_SAMPLE_WINDOW_MS)).await;

    let mut manager = audio_state.lock().await;
    let mut restarted: Vec<AudioSourceLabel> = Vec::new();

    if check_mic {
        let mic_now = manager.mic_write_pos();
        let stalled = mic_now == mic_initial && should_stall_restart(&AudioSourceLabel::Mic);
        // Default-device-change preflight is now handled by the device
        // broker's `RestartIntent` path; preflight only catches stream
        // errors and write-position stalls left over from idle time.
        if mic_err || stalled {
            warn!(
                "preflight: mic stream needs restart (error={}, stalled={})",
                mic_err, stalled
            );
            match manager.restart_mic(yapstack_audio::manager::RestartTarget::PreserveBinding) {
                Ok(_) => restarted.push(AudioSourceLabel::Mic),
                Err(e) => {
                    error!("preflight: mic restart failed: {}", e);
                    return Err(CommandError::from(e));
                }
            }
        }
    }

    if check_system {
        let sys_now = manager.system_write_pos();
        let stalled = sys_now == sys_initial && should_stall_restart(&AudioSourceLabel::System);
        if sys_err || stalled {
            warn!(
                "preflight: system stream needs restart (error={}, stalled={})",
                sys_err, stalled
            );
            match manager.restart_system_audio() {
                Ok(_) => restarted.push(AudioSourceLabel::System),
                Err(e) => {
                    error!("preflight: system restart failed: {}", e);
                    if matches!(source, CaptureSource::SystemOnly) {
                        return Err(CommandError::from(e));
                    }
                    // Mixed mode: degrade to mic-only, surface a health event.
                    let _ = app_handle.emit(
                        "stream-health",
                        StreamHealthEvent {
                            source: AudioSourceLabel::System,
                            status: "restart_failed".into(),
                            message: format!("preflight system restart failed: {e}"),
                            bound_device_name: None,
                        },
                    );
                }
            }
        }
    }

    drop(manager);

    for label in restarted {
        let name = source_display_name(&label);
        let bound_device_name = {
            let manager = audio_state.lock().await;
            match label {
                AudioSourceLabel::Mic => manager.mic_bound_device().map(|s| s.to_string()),
                AudioSourceLabel::System => {
                    manager.system_audio_bound_device().map(|s| s.to_string())
                }
            }
        };
        let _ = app_handle.emit(
            "stream-health",
            StreamHealthEvent {
                source: label,
                status: "restarted".into(),
                message: format!("{name} stream restarted (preflight)"),
                bound_device_name,
            },
        );
    }

    Ok(())
}

/// Classification of consecutive empty WAV flush extractions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmptyFlushClassification {
    /// Normal Windows WASAPI silence — no system audio playing.
    WindowsSilence,
    /// Real extraction failure — possible stream error or sample-rate mismatch.
    Error,
}

/// Classifies consecutive empty WAV flush extractions. On Windows, system-only sessions
/// produce zero samples when nothing is playing (WASAPI event-driven loopback). This is
/// normal and should not be reported as an error unless the cpal error flag is set.
fn classify_empty_flush(source: CaptureSource, has_stream_error: bool) -> EmptyFlushClassification {
    let is_windows_system_silence = cfg!(target_os = "windows")
        && matches!(source, CaptureSource::SystemOnly)
        && !has_stream_error;
    if is_windows_system_silence {
        EmptyFlushClassification::WindowsSilence
    } else {
        EmptyFlushClassification::Error
    }
}

/// One chunk produced by the backfill VAD simulation — carries the start
/// sample index (for computing audio_offset_seconds relative to the backfill
/// window) and the sample slice range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VadBackfillChunk {
    start: usize,
    end: usize,
}

/// Simulate the live VAD state machine over a historical audio buffer and
/// produce chunks with the same boundaries the live loop would have picked.
/// Using the same `VadTuning` the live loop uses gives backfill and live
/// segmentation identical character — a session's first N seconds no longer
/// look subtly different from the rest.
///
/// Runs Silero VAD across the buffer up-front (one probability per 32 ms
/// frame), then walks the probability stream with the same state machine
/// the live loop uses:
/// - is_speaking (onset at `tuning.speech_threshold`, offset at
///   `tuning.speech_threshold * tuning.offset_threshold_ratio` — hysteresis)
/// - silence_run / speech_run counted in Silero frames
/// - pre-roll rewinds the chunk start by `tuning.pre_roll`
///
/// Any trailing speech that didn't naturally end in silence is emitted as a
/// final chunk so the tail of the backfill window is never dropped. If
/// Silero init fails (e.g. ort runtime unavailable), falls back to treating
/// the whole buffer as a single chunk — better than silently losing the
/// backfill audio.
fn vad_chunk_historical_audio(
    samples: &[f32],
    sample_rate: u32,
    tuning: &VadTuning,
) -> Vec<VadBackfillChunk> {
    if samples.is_empty() {
        return Vec::new();
    }

    let mut silero = match SileroVad::new() {
        Ok(v) => v,
        Err(e) => {
            warn!("backfill: Silero init failed ({e}) — single-chunk fallback");
            return split_backfill_chunks(
                vec![VadBackfillChunk {
                    start: 0,
                    end: samples.len(),
                }],
                sample_rate,
            );
        }
    };

    let probabilities = silero.score_all(samples, sample_rate);
    if probabilities.is_empty() {
        return Vec::new();
    }

    split_backfill_chunks(
        backfill_chunks_from_probabilities(&probabilities, samples.len(), sample_rate, tuning),
        sample_rate,
    )
}

fn split_backfill_chunks(chunks: Vec<VadBackfillChunk>, sample_rate: u32) -> Vec<VadBackfillChunk> {
    let max_samples = (BACKFILL_JOB_QUANTUM_SECS * sample_rate as f32) as usize;
    if max_samples == 0 {
        return chunks;
    }

    let mut split = Vec::new();
    for chunk in chunks {
        let mut start = chunk.start;
        while start < chunk.end {
            let end = start.saturating_add(max_samples).min(chunk.end);
            split.push(VadBackfillChunk { start, end });
            start = end;
        }
    }
    split
}

/// Pure state-machine half of `vad_chunk_historical_audio`: walk a
/// pre-computed Silero probability stream and produce the same chunk
/// boundaries the live loop would have picked. Extracted so tests can
/// exercise the state machine with hand-crafted probability sequences
/// without loading the ONNX model.
fn backfill_chunks_from_probabilities(
    probabilities: &[f32],
    total_samples: usize,
    sample_rate: u32,
    tuning: &VadTuning,
) -> Vec<VadBackfillChunk> {
    if probabilities.is_empty() || total_samples == 0 {
        return Vec::new();
    }
    // One probability covers FRAME_DURATION_SECS (32 ms) of audio. Map
    // each frame back to the corresponding slice in the *original*
    // (un-resampled) buffer so chunk boundaries are expressed in the
    // caller's coordinate system.
    let frame_samples_original =
        (super::silero_vad::FRAME_DURATION_SECS * sample_rate as f32) as usize;
    if frame_samples_original == 0 {
        return Vec::new();
    }

    let onset_threshold = tuning.speech_threshold;
    let offset_threshold = tuning.speech_threshold * tuning.offset_threshold_ratio;
    let silence_windows = (tuning.silence_duration.as_secs_f32()
        / super::silero_vad::FRAME_DURATION_SECS)
        .ceil() as usize;
    let max_chunk_windows = (tuning.max_chunk_duration.as_secs_f32()
        / super::silero_vad::FRAME_DURATION_SECS)
        .max(1.0) as usize;
    let pre_roll_samples = (tuning.pre_roll.as_secs_f32() * sample_rate as f32) as usize;

    let mut chunks: Vec<VadBackfillChunk> = Vec::new();
    let mut is_speaking = false;
    let mut speech_start: usize = 0;
    let mut silence_run: usize = 0;
    let mut speech_run: usize = 0;
    // Pre-roll must not rewind past the last emitted chunk's end, or the
    // next chunk would re-transcribe the tail of the previous one.
    let mut prev_chunk_end: usize = 0;

    for (frame_idx, &prob) in probabilities.iter().enumerate() {
        let w_start = frame_idx * frame_samples_original;
        let w_end = ((frame_idx + 1) * frame_samples_original).min(total_samples);
        if w_start >= total_samples {
            break;
        }

        if is_speaking {
            speech_run += 1;
            let is_loud = prob >= offset_threshold;
            if is_loud {
                silence_run = 0;
                if speech_run >= max_chunk_windows {
                    chunks.push(VadBackfillChunk {
                        start: speech_start,
                        end: w_end,
                    });
                    prev_chunk_end = w_end;
                    speech_start = w_end;
                    speech_run = 0;
                }
            } else {
                silence_run += 1;
                if silence_run >= silence_windows {
                    chunks.push(VadBackfillChunk {
                        start: speech_start,
                        end: w_end,
                    });
                    prev_chunk_end = w_end;
                    is_speaking = false;
                    speech_run = 0;
                    silence_run = 0;
                }
            }
        } else {
            let is_loud = prob >= onset_threshold;
            if is_loud {
                speech_start = w_start.saturating_sub(pre_roll_samples).max(prev_chunk_end);
                is_speaking = true;
                speech_run = 1;
                silence_run = 0;
            }
        }
    }

    // Trailing speech that never resolved into silence — emit as final chunk
    // so we don't lose the tail of the backfill window.
    if is_speaking && speech_start < total_samples {
        chunks.push(VadBackfillChunk {
            start: speech_start,
            end: total_samples,
        });
    }

    chunks
}

/// Process backfill audio concurrently with the live VAD loop. Each source's
/// chunk boundaries come from `vad_chunk_historical_audio`, which runs the
/// same Silero-driven state machine the live loop uses; backfill and live
/// segmentation share boundary choices (no quality gap between a session's
/// first N seconds and the rest). Emits segments with offsets in the backfill
/// window, then sets `backfill_done`.
///
/// `counters` is the same `Arc<StdMutex<SessionCounters>>` the live loop
/// holds — backfill chunks update the same totals the live loop and
/// `get_live_transcription_status` read, so the status surface reflects
/// backfill + live work together. Backfill owns its **own** `PromptState`
/// (built locally below) so the prompt-bridge step at end-of-backfill can't
/// race with concurrent live writes — once bridging is done, the bridged
/// text flows through `ctx.bridged_prompt` to the live loop's
/// `seed_prompt_from_backfill` instead.
async fn process_backfill(
    ctx: TranscriptionContext,
    backfill_audio: Vec<(Vec<f32>, u32, AudioSourceLabel)>,
    backfill_done: Arc<AtomicBool>,
    tuning: VadTuning,
    counters: Arc<StdMutex<SessionCounters>>,
) {
    info!("backfill: starting concurrent processing");

    // Build per-source chunk lists using the live VAD state machine. This
    // gives backfill and live segmentation identical boundary choices —
    // no more quality gap between a session's first N seconds (historical)
    // and the rest (live). Uses the same engine-keyed `VadTuning`, so a
    // Parakeet backfill gets dialogue-tuned cadence + pre-roll.
    let mut source_entries: Vec<VadBackfillSource> = Vec::new();
    for (samples, sample_rate, label) in &backfill_audio {
        let chunks = vad_chunk_historical_audio(samples, *sample_rate, &tuning);
        debug!(
            "backfill: source={:?} {} chunks from {} samples ({:.1}s)",
            label,
            chunks.len(),
            samples.len(),
            samples.len() as f32 / *sample_rate as f32
        );
        source_entries.push(VadBackfillSource {
            label: *label,
            samples: samples.clone(),
            sample_rate: *sample_rate,
            chunks,
        });
    }

    // Per-source state
    let mut chunk_indices: Vec<u32> = vec![0; source_entries.len()];
    let mut accumulated_texts: Vec<String> = vec![String::new(); source_entries.len()];

    // Backfill-local prompt accumulator. Kept separate from the live loop's
    // `PromptState` so the bridge step below can `take` it without racing
    // concurrent live writes.
    let backfill_prompt = Arc::new(StdMutex::new(PromptState {
        shared_prompt: String::new(),
        last_transcription_at: None,
    }));

    // Interleave: process chunk 0 for all sources, then chunk 1, etc. Keeps
    // cross-source timestamps roughly interleaved instead of emitting all
    // of one source before the other.
    let total_chunks = source_entries
        .iter()
        .map(|s| s.chunks.len())
        .max()
        .unwrap_or(0);

    'outer: for chunk_idx in 0..total_chunks {
        for (source_idx, source) in source_entries.iter().enumerate() {
            let Some(bounds) = source.chunks.get(chunk_idx) else {
                continue;
            };
            let slice = &source.samples[bounds.start..bounds.end];
            let audio_offset = bounds.start as f32 / source.sample_rate as f32;
            debug!(
                "backfill chunk: source={:?} idx={} offset={:.2}s samples={} ({:.2}s)",
                source.label,
                chunk_idx,
                audio_offset,
                slice.len(),
                slice.len() as f32 / source.sample_rate as f32
            );
            let input = ChunkInput {
                samples: slice,
                sample_rate: source.sample_rate,
                audio_offset_seconds: audio_offset,
                source_label: source.label,
                drain_backlog_seconds: 0.0,
                origin: JobOrigin::Backfill,
            };
            let outcome = transcribe_and_emit_chunk(
                &ctx,
                &input,
                &mut chunk_indices[source_idx],
                &mut accumulated_texts[source_idx],
                &counters,
                &backfill_prompt,
            )
            .await;
            // Surface a dead sidecar early so we don't keep submitting
            // backfill chunks that will all fail. Skipped/Cancelled outcomes
            // are non-fatal: a Cancelled chunk only happens if the scheduler
            // is shutting down (then there's no more useful work to do
            // anyway), and Skipped is a transient engine error.
            if matches!(outcome, TranscribeOutcome::SidecarDead) {
                warn!("backfill: scheduler reports sidecar dead — aborting backfill drain");
                break 'outer;
            }
        }
    }

    {
        let s = counters.lock().expect("counters mutex poisoned");
        info!(
            "backfill: completed {} chunks, {:.1}s audio",
            s.total_chunks, s.total_audio_seconds
        );
    }

    // Bridge backfill's accumulated prompt out to the live loop. Since
    // `backfill_prompt` is owned only by this task, taking from it here
    // can't race with live chunks.
    {
        let mut bridged = ctx.bridged_prompt.lock().await;
        let max_prompt = ctx.config.prompt_context_chars.unwrap_or(350) as usize;
        let mut bp = backfill_prompt
            .lock()
            .expect("backfill prompt mutex poisoned");
        if bp.shared_prompt.len() > max_prompt {
            let boundary = bp
                .shared_prompt
                .ceil_char_boundary(bp.shared_prompt.len() - max_prompt);
            *bridged = bp.shared_prompt[boundary..].to_string();
        } else {
            *bridged = std::mem::take(&mut bp.shared_prompt);
        }
    }

    // Signal that backfill is done — live loop can now count empty flushes toward error threshold
    backfill_done.store(true, Ordering::Release);

    // Signal frontend that backfill processing is done. Suppressed for
    // dictation runtimes — dictation has `backfill_seconds == 0` so there's
    // nothing meaningful to signal, and emitting would clobber session
    // backfill UI state when both are running concurrently.
    if matches!(ctx.config.source_kind, LiveSourceKind::Session) {
        let _ = ctx.app_handle.emit("backfill-complete", ());
    }
}

/// Returns a human-readable name for a source label.
fn source_display_name(label: &AudioSourceLabel) -> &'static str {
    match label {
        AudioSourceLabel::Mic => "Microphone",
        AudioSourceLabel::System => "System audio",
    }
}

/// Per-tick stream-health pass over every source. Each source is checked
/// against three layered signals — OS-authoritative listener events first,
/// then symptom-based detection (cpal error flag, write-position stall,
/// device-identity drift) gated by a cooldown — and restarted when any one
/// of them fires. `session_wav_state` is threaded through so a
/// buffer-replacing restart can reset the WAV writer's flush position;
/// otherwise the old position against the fresh buffer stalls writes until
/// the new counter climbs past it.
#[allow(clippy::too_many_arguments)]
async fn check_stream_health(
    sources: &mut [SourceVadState],
    audio_state: &AudioManagerState,
    ctx: &TranscriptionContext,
    counters: &Arc<StdMutex<SessionCounters>>,
    prompt: &Arc<StdMutex<PromptState>>,
    chunk_tasks: &mut futures_util::stream::FuturesUnordered<ChunkTaskFuture>,
    pending_outcomes: &mut Vec<ChunkTaskOutcome>,
    mut session_wav_state: Option<&mut SessionWavState>,
    is_mixed: bool,
) -> bool {
    for source in sources.iter_mut() {
        if source.restart_attempts >= STREAM_RESTART_MAX_ATTEMPTS {
            continue;
        }

        // OS-driven default-device changes are handled by the device
        // broker, which routes a `RestartIntent` into the live loop's
        // inbox. The remaining symptom-based watchdog (cpal error flag
        // + write-pos stall) lives here as a backup for stream deaths
        // the broker can't see.
        let in_cooldown = source
            .last_restart_at
            .is_some_and(|t| t.elapsed() < Duration::from_secs_f32(STREAM_RESTART_COOLDOWN_SECS));
        if in_cooldown {
            continue;
        }
        let Some(reason) = evaluate_speculative_signals(source, audio_state).await else {
            continue;
        };
        // `as_deref_mut` reborrows the `Option<&mut _>` so each loop
        // iteration gets a fresh mutable view without consuming the slot.
        #[allow(clippy::needless_option_as_deref)]
        let ws_borrow = session_wav_state.as_deref_mut();
        // Watchdog-driven restart: cpal error or write-pos stall. The
        // bound device is presumed still correct; just the stream died.
        let report = attempt_source_restart(
            source,
            audio_state,
            ctx,
            &reason,
            yapstack_audio::manager::RestartTarget::PreserveBinding,
        )
        .await;
        if let Some(mut report) = report {
            if report.outcome == yapstack_audio::manager::RestartOutcome::BufferReplaced {
                // `.take()` moves the Arc out of the report; the manager's
                // own handle has already been dropped, so this is the only
                // remaining reference to the old buffer (callers own the
                // unique handle once they observe `BufferReplaced`).
                let previous_buffer = report.previous_buffer.take();
                handle_buffer_replacement(
                    source,
                    audio_state,
                    ctx,
                    counters,
                    prompt,
                    chunk_tasks,
                    pending_outcomes,
                    previous_buffer,
                    ws_borrow,
                    source_display_name(&source.label),
                )
                .await;
            }
        }
    }

    // Mixed mid-capture fail-fast. The user's intent with Mixed is
    // "capture both", not "limp along on the surviving Source", so a
    // terminal restart failure on either side ends the whole Capture.
    // The per-source `restart_abandoned` toast was already emitted from
    // inside `attempt_source_restart`; no extra event needed here.
    is_mixed
        && sources
            .iter()
            .any(|s| s.restart_attempts >= STREAM_RESTART_MAX_ATTEMPTS)
}

/// Symptom-based stream-failure detection, gated by the speculative-restart
/// cooldown in the caller. Layer 1 is the cpal error-callback flag (instant);
/// Layer 2 is the write-position stall watchdog (~2 s). Default-device
/// changes are handled by the device broker, not here. Returns the first
/// reason that fires, or `None` if none do.
async fn evaluate_speculative_signals(
    source: &mut SourceVadState,
    audio_state: &AudioManagerState,
) -> Option<String> {
    let manager = audio_state.lock().await;

    // Layer 1: cpal error callback flag.
    let has_error = match source.label {
        AudioSourceLabel::Mic => manager.mic_has_stream_error(),
        AudioSourceLabel::System => manager.system_has_stream_error(),
    };
    if has_error {
        return Some("stream error callback fired".into());
    }

    // Layer 2: write_pos stall detection. On Windows, system audio loopback
    // produces zero samples when nothing is playing — that's normal WASAPI
    // behavior, so `should_stall_restart` skips that case.
    let current_pos = source_write_pos(&manager, &source.label);
    if current_pos > source.last_seen_write_pos {
        source.last_seen_write_pos = current_pos;
        source.last_write_pos_advance = Instant::now();
    } else if source.last_write_pos_advance.elapsed()
        > Duration::from_secs_f32(STREAM_STALL_THRESHOLD_SECS)
        && should_stall_restart(&source.label)
    {
        return Some("write position stalled".into());
    }

    None
}

/// Run a single restart attempt for `source`. Records the attempt timestamp,
/// invokes the engine's restart entry point, and on success handles the
/// buffer-replacement bookkeeping (cursor reset + WAV flush-position reset).
/// On a same-device rebind, treats the attempt as partial — increments the
/// retry counter and clears the cooldown so the next tick can try again
/// immediately instead of waiting 5 s. Emits a `stream-health` event in
/// every outcome.
/// Run a single stream-restart attempt and update `source.restart_attempts`
/// / `last_restart_at` bookkeeping. **Does not** handle buffer replacement —
/// the caller inspects the returned `RestartReport` and, if
/// `outcome == BufferReplaced`, calls `handle_buffer_replacement` with the
/// full set of live-loop state it needs (`chunk_tasks`, `pending_outcomes`,
/// `session_wav_state`). Splitting these responsibilities lets the rescue
/// path coordinate with the in-flight task drain without re-plumbing those
/// fields through the stream-error code path.
///
/// Returns `Some(report)` on success and `None` on error (where
/// `restart_attempts` was already incremented and a `stream-health` event
/// emitted). Same-device rebinds still return `Some` because they're a
/// successful manager call — the caller's `BufferReplaced` check will be
/// false so no rescue runs, but the stream-health event is emitted.
async fn attempt_source_restart(
    source: &mut SourceVadState,
    audio_state: &AudioManagerState,
    ctx: &TranscriptionContext,
    reason: &str,
    target: yapstack_audio::manager::RestartTarget,
) -> Option<yapstack_audio::manager::RestartReport> {
    let app_handle = &ctx.app_handle;
    let source_name = source_display_name(&source.label);
    warn!(
        "stream health: {} needs restart ({}, target={:?}), attempt {}/{}",
        source_name,
        reason,
        target,
        source.restart_attempts + 1,
        STREAM_RESTART_MAX_ATTEMPTS
    );

    source.last_restart_at = Some(Instant::now());

    let restart_result = {
        let mut manager = audio_state.lock().await;
        match source.label {
            AudioSourceLabel::Mic => manager.restart_mic(target),
            // System audio always uses cpal's default output — there is
            // no explicit-output picking to preserve, so the target
            // parameter is structurally a no-op for this path.
            AudioSourceLabel::System => manager.restart_system_audio(),
        }
    };

    match restart_result {
        Ok(report) => {
            info!(
                "stream health: {} restarted successfully (outcome: {:?}, same_device: {}, new_id: {:?})",
                source_name, report.outcome, report.same_device, report.new_device_id
            );

            // Same-device rebind likely means macOS was still settling after
            // a Bluetooth handshake (cpal#1175). Treat it like a partial
            // failure: keep counting attempts and clear the cooldown so the
            // next tick can retry immediately. Do NOT reset
            // last_write_pos_advance — the stream hasn't actually recovered,
            // and Layer 2 should fire on the next tick if write_pos isn't
            // advancing.
            if report.same_device {
                warn!(
                    "{} restart rebound to the same device ({:?}) — macOS likely still settling; \
                     retrying on next poll (attempt {}/{})",
                    source_name,
                    report.new_device_id,
                    source.restart_attempts + 1,
                    STREAM_RESTART_MAX_ATTEMPTS
                );
                source.restart_attempts += 1;
                source.last_restart_at = None;
            } else {
                source.restart_attempts = 0;
                source.last_write_pos_advance = Instant::now();
            }

            let _ = app_handle.emit(
                "stream-health",
                StreamHealthEvent {
                    source: source.label,
                    status: "restarted".into(),
                    message: format!("{} stream restarted ({})", source_name, reason),
                    bound_device_name: report.bound_device_name.clone(),
                },
            );
            Some(report)
        }
        Err(e) => {
            source.restart_attempts += 1;
            error!(
                "stream health: {} restart failed (attempt {}): {}",
                source_name, source.restart_attempts, e
            );
            let status = if source.restart_attempts >= STREAM_RESTART_MAX_ATTEMPTS {
                "restart_abandoned"
            } else {
                "restart_failed"
            };
            let _ = app_handle.emit(
                "stream-health",
                StreamHealthEvent {
                    source: source.label,
                    status: status.into(),
                    message: format!(
                        "{} stream restart failed: {} (attempt {}/{})",
                        source_name, e, source.restart_attempts, STREAM_RESTART_MAX_ATTEMPTS
                    ),
                    bound_device_name: None,
                },
            );
            None
        }
    }
}

/// Buffer-replaced bookkeeping. Coordinates the full set of state that gets
/// invalidated when a ring buffer is swapped out from under us, in an order
/// that preserves audio continuity *and* the per-source one-in-flight-task
/// invariant.
///
/// Steps (in order):
///
/// 1. **Snapshot the swap instant** — read the new buffer's `write_pos` and
///    sample `session_start_instant.elapsed()` together, before any other
///    await. The new buffer received its first samples at the manager's
///    `restart_*` return; reading `write_pos` *now* picks up whatever cpal
///    has written since, and pairing it with `elapsed()` locks the session-
///    time → buffer-position bijection at the swap moment. Doing this
///    *before* the in-flight drain and the pre-swap rescue prevents the
///    seconds of slow await work from shifting the new anchor forward and
///    silently discarding audio captured during the rescue window.
///
/// 2. **Drain the source's in-flight chunk task**, if any, so the rescue
///    dispatch below sees the latest `accumulated_text` and doesn't race
///    a still-running task's outcome. Other sources' outcomes that arrive
///    in the meantime are pushed onto `pending_outcomes` so the next normal
///    loop tick applies them — preserves the global drain contract.
///
/// 3. **Rescue pre-swap audio** by spawning a `FinalFlush` chunk into
///    `chunk_tasks` with the *old* anchor and old source format. The chunk
///    starts at `speech_start_pos` (not `cursor`) so a pending utterance
///    that hasn't been dispatched yet — typically due to pre-roll
///    extending the start before the latest extraction, or a previous
///    too-short extraction — isn't truncated at the front. This goes
///    through the same chunk_tasks pipeline as a normal dispatch
///    (`mem::take(accumulated_text)`, `has_in_flight_task = true`,
///    `chunk_index` increment), so the global text-restoration contract
///    holds.
///
/// 4. **Rescue session-WAV audio** by reading `[ws.flush_positions.*pos,
///    prev_buf.write_pos)` from `previous_buffer` and appending it to the
///    WAV with resampling. Scope: `MicOnly` and `SystemOnly` capture
///    sources; `Mixed` logs a warning and skips (rescuing the mix would
///    require both sources' old buffers at the same instant and is out of
///    scope for V1).
///
/// 5. **Reset position state and re-anchor** using the values snapshotted
///    in step 1. After this, the source's `session_start_pos = new_pos@swap`
///    and `audio_offset_anchor_seconds = new_anchor@swap`. Any audio
///    captured between the swap instant and now is past `session_start_pos`
///    and will be picked up by the next chunk extraction.
///
/// 6. **Purge `dictation_owns_mic` on Mic-Session.** Closed and open
///    windows store positions in the old mic ring's coordinate space; the
///    next session tick would consume them against the new buffer and call
///    `reset_vad_past(old_end)` which is past the new buffer's `write_pos`.
///    `clear_for_session` drops both. Any dictation in flight at the moment
///    of swap loses its session-side bookkeeping — the dictation runtime
///    handles its own `BufferReplaced` event independently. Also clears
///    this source's `dictation_consumed_through` / `flushed_open_at` for
///    the same reason.
///
/// 7. **Update WAV flush_positions** to `new_pos@swap`. Done last so the
///    step-4 rescue isn't re-extracted on the next tick.
#[allow(clippy::too_many_arguments)]
async fn handle_buffer_replacement(
    source: &mut SourceVadState,
    audio_state: &AudioManagerState,
    ctx: &TranscriptionContext,
    counters: &Arc<StdMutex<SessionCounters>>,
    prompt: &Arc<StdMutex<PromptState>>,
    chunk_tasks: &mut futures_util::stream::FuturesUnordered<ChunkTaskFuture>,
    pending_outcomes: &mut Vec<ChunkTaskOutcome>,
    previous_buffer: Option<yapstack_audio::SharedAudioRingBuffer>,
    mut session_wav_state: Option<&mut SessionWavState>,
    source_name: &str,
) {
    // (1) Snapshot the swap instant.
    let (new_pos_at_swap, new_sr, new_ch) = {
        let manager = audio_state.lock().await;
        let info = match source.label {
            AudioSourceLabel::Mic => manager.mic_buffer_info(),
            AudioSourceLabel::System => manager.system_buffer_info(),
        };
        let pos = source_write_pos(&manager, &source.label);
        info.map(|i| (pos, i.sample_rate, i.channels)).unwrap_or((
            pos,
            source.source_sample_rate,
            source.source_channels,
        ))
    };
    let elapsed_secs = ctx.session_start_instant.elapsed().as_secs_f32();
    let new_anchor = ctx.session_offset_base_seconds + elapsed_secs;

    // Snapshot every pre-swap value used below before mutating `source`.
    let old_anchor = source.audio_offset_anchor_seconds;
    let old_session_start_pos = source.session_start_pos;
    // Pre-swap rescue extracts from `speech_start_pos`, not `cursor`: a
    // pending utterance can have `speech_start_pos < cursor` after a
    // pre-roll rewind or a too-short extraction (see prepare_chunk_dispatch_inner).
    let old_speech_start_pos = source.speech_start_pos;
    let old_sr = source.source_sample_rate;
    let old_ch = source.source_channels;
    let source_label = source.label;

    // (1a) Mic-Session: rebase dictation_owns_mic *before* the rescues.
    // Capture the dropped closed windows and the prior open start in OLD
    // coordinates so the rescue helpers can truncate their extraction
    // range at the earliest dictation boundary inside it (dictated audio
    // must never leak into session content). Rebasing the open window's
    // start to `new_pos_at_swap` keeps the in-flight dictation safe — the
    // dictation runtime experiences its own buffer-replacement event on
    // its own SourceVadState and will record the eventual `close(end)` in
    // new-buffer coordinates, matching the rebased start. This replaces
    // the previous `clear_for_session()` call which was unsafe while a
    // dictation was actively owning the mic (it would unwind the open
    // window and let dictated audio leak into subsequent session segments).
    let dictation_rebase = if matches!(source.label, AudioSourceLabel::Mic)
        && matches!(ctx.config.source_kind, LiveSourceKind::Session)
    {
        Some(
            ctx.dictation_owns_mic
                .rebase_for_mic_buffer_replacement(new_pos_at_swap as u64),
        )
    } else {
        None
    };
    // Earliest dictation boundary inside [old_speech_start_pos, old_write_pos).
    // Used to truncate both the transcript rescue and the WAV rescue so
    // dictated audio captured before the swap doesn't end up in the
    // session's transcript or recording. None means the rescue can run
    // to the full extent of the old buffer.
    let dictation_truncate_at = dictation_rebase.as_ref().and_then(|r| {
        let range_end_hint = previous_buffer
            .as_ref()
            .map(|b| b.samples_written())
            .unwrap_or(old_speech_start_pos);
        earliest_dictation_start_in_range(old_speech_start_pos, range_end_hint, r)
    });

    // (2) Drain in-flight task for this source so the rescue's
    // `mem::take(accumulated_text)` and `has_in_flight_task` writes don't
    // race a still-running task's outcome.
    if source.has_in_flight_task {
        drain_in_flight_for_source(source, chunk_tasks, pending_outcomes).await;
    }

    // (3) Spawn the pre-swap transcription rescue.
    let rescue_audio = previous_buffer.as_ref().and_then(|prev| {
        spawn_pre_swap_rescue_chunk(
            source,
            prev,
            old_anchor,
            old_session_start_pos,
            old_speech_start_pos,
            old_sr,
            old_ch,
            dictation_truncate_at,
            ctx,
            counters,
            prompt,
            chunk_tasks,
        )
    });

    // (4) Rescue session-WAV audio from the old buffer (MicOnly / SystemOnly).
    let wav_rescue = if let (Some(prev), Some(ws)) =
        (previous_buffer.as_ref(), session_wav_state.as_deref_mut())
    {
        rescue_pre_swap_wav_audio(ws, prev, source.label, dictation_truncate_at, source_name)
    } else {
        None
    };

    // (5) Reset position state and re-anchor.
    source.reset_for_buffer_replacement(new_pos_at_swap, new_sr, new_ch, new_anchor);

    // (6) Mic-Session: clear the source's local mic-ownership cursors.
    // dictation_owns_mic itself was already rebased in step (1a) — the
    // open window (if any) now points into the new buffer's coordinate
    // space, the dropped closed windows are consumed by the rescue
    // truncation, and no `clear_for_session` is needed.
    if matches!(source.label, AudioSourceLabel::Mic)
        && matches!(ctx.config.source_kind, LiveSourceKind::Session)
    {
        source.dictation_consumed_through = 0;
        source.flushed_open_at = None;
    }

    // (7) Update WAV flush positions and log.
    let (rescue_samples, rescue_duration_secs) = rescue_audio.unwrap_or((0, 0.0));
    let (wav_rescue_samples, wav_rescue_duration_secs) = wav_rescue.unwrap_or((0, 0.0));
    let dictation_rebase_info = dictation_rebase.as_ref();
    info!(
        marker = "buffer_replaced_rebase",
        source = ?source_label,
        old_anchor_secs = old_anchor,
        new_anchor_secs = new_anchor,
        anchor_delta_secs = new_anchor - old_anchor,
        elapsed_in_loop_secs = elapsed_secs,
        rescue_chunk_samples = rescue_samples,
        rescue_chunk_duration_secs = rescue_duration_secs,
        wav_rescue_samples,
        wav_rescue_duration_secs,
        new_write_pos = new_pos_at_swap,
        new_sample_rate = new_sr,
        new_channels = new_ch,
        dictation_dropped_closed = dictation_rebase_info.map(|r| r.dropped_closed.len()).unwrap_or(0),
        dictation_prior_open = ?dictation_rebase_info.and_then(|r| r.prior_open),
        dictation_new_open = ?dictation_rebase_info.and_then(|r| r.new_open),
        dictation_truncate_at = ?dictation_truncate_at,
        "rebased source on buffer replacement"
    );
    warn!(
        "{} buffer replaced on restart ({}Hz/{}ch) — source state reset",
        source_name, new_sr, new_ch
    );

    if let Some(ws) = session_wav_state {
        match source.label {
            AudioSourceLabel::Mic => ws.flush_positions.mic_pos = new_pos_at_swap,
            AudioSourceLabel::System => ws.flush_positions.system_pos = new_pos_at_swap,
        }
        if new_sr != ws.wav_sample_rate {
            info!(
                "WAV sample-rate mismatch on {} rebind: writer={}Hz, new buffer={}Hz — extracted samples will be resampled on write",
                source_name, ws.wav_sample_rate, new_sr
            );
        }
    }
}

/// Earliest mic-buffer position inside `[range_start, range_end)` at which a
/// dictation window begins. Used to truncate a pre-swap rescue range so
/// dictated audio doesn't leak into session content.
///
/// Walks both the closed windows just drained by
/// `DictationOwnsMic::rebase_for_mic_buffer_replacement` and the prior open
/// start. All positions are in OLD mic-ring coordinates, same as the
/// rescue range.
fn earliest_dictation_start_in_range(
    range_start: usize,
    range_end: usize,
    rebase: &super::transcription::DictationRebaseResult,
) -> Option<usize> {
    if range_start >= range_end {
        return None;
    }
    let mut earliest: Option<u64> = None;
    let consider = |earliest: &mut Option<u64>, pos: u64| {
        if (pos as usize) >= range_start && (pos as usize) < range_end {
            *earliest = Some(earliest.map_or(pos, |e: u64| e.min(pos)));
        }
    };
    for w in &rebase.dropped_closed {
        consider(&mut earliest, w.start);
    }
    if let Some(open_start) = rebase.prior_open {
        consider(&mut earliest, open_start);
    }
    earliest.map(|e| e as usize)
}

/// Drain `chunk_tasks` until the in-flight task for `source.label` lands.
/// Other sources' outcomes are queued onto `pending_outcomes` so the next
/// normal loop tick applies them; we can't drop them or apply them inline
/// because each source's outcome must be matched against its own
/// `SourceVadState`. Returns early if `chunk_tasks` is unexpectedly empty —
/// the `has_in_flight_task` flag is set in lockstep with the push, so this
/// only happens on programmer error and is treated as a soft failure
/// (clear the flag and continue).
async fn drain_in_flight_for_source(
    source: &mut SourceVadState,
    chunk_tasks: &mut futures_util::stream::FuturesUnordered<ChunkTaskFuture>,
    pending_outcomes: &mut Vec<ChunkTaskOutcome>,
) {
    use futures_util::stream::StreamExt;
    while source.has_in_flight_task {
        let Some(outcome) = chunk_tasks.next().await else {
            warn!(
                marker = "buffer_replaced_drain_empty",
                source = ?source.label,
                "expected in-flight chunk task on source, but chunk_tasks is empty; \
                 clearing has_in_flight_task to unblock"
            );
            source.has_in_flight_task = false;
            return;
        };
        if outcome.source_label == source.label {
            source.accumulated_text = outcome.accumulated_text;
            source.has_in_flight_task = false;
            if outcome.sidecar_dead {
                // Queue so the main loop's `fatal` check sees it. Use
                // pending_outcomes (same as the normal drain path).
                pending_outcomes.push(ChunkTaskOutcome {
                    source_label: outcome.source_label,
                    accumulated_text: source.accumulated_text.clone(),
                    sidecar_dead: true,
                });
            }
        } else {
            pending_outcomes.push(outcome);
        }
    }
}

/// Extract `[old_speech_start_pos, old_write_pos)` from the just-swapped-out
/// ring, deinterleave to mono, and **spawn** a `FinalFlush` chunk into
/// `chunk_tasks`. Uses the *old* anchor / sample rate / channel count so the
/// resulting segment's `audio_offset_seconds` reflects pre-swap session-time
/// even though the transcribe happens after the swap. Mutates source state
/// exactly the way a normal `prepare_chunk_dispatch` call does (taking
/// `accumulated_text`, bumping `chunk_index`, setting `has_in_flight_task =
/// true`) — the global text-restoration contract relies on this so a later
/// outcome can apply cleanly.
///
/// Returns `Some((sample_count, duration_secs))` when a chunk was queued,
/// `None` for empty / sub-threshold extractions. Caller must have already
/// drained any prior in-flight task on this source (see
/// `drain_in_flight_for_source`) — this helper assumes
/// `source.has_in_flight_task == false`.
#[allow(clippy::too_many_arguments)]
fn spawn_pre_swap_rescue_chunk(
    source: &mut SourceVadState,
    previous_buffer: &yapstack_audio::SharedAudioRingBuffer,
    old_anchor: f32,
    old_session_start_pos: usize,
    old_speech_start_pos: usize,
    old_sr: u32,
    old_ch: u16,
    // Earliest mic-buffer position inside the rescue range where a
    // dictation window begins. The rescue is clamped to end here so
    // dictated audio doesn't leak into the session transcript. `None`
    // means no dictation overlap — extract through to the old write
    // position. Always `None` for non-mic sources (System audio is
    // never dictated).
    dictation_truncate_at: Option<usize>,
    ctx: &TranscriptionContext,
    counters: &Arc<StdMutex<SessionCounters>>,
    prompt: &Arc<StdMutex<PromptState>>,
    chunk_tasks: &mut futures_util::stream::FuturesUnordered<ChunkTaskFuture>,
) -> Option<(usize, f32)> {
    let old_write_pos = previous_buffer.samples_written();
    let effective_end = dictation_truncate_at
        .map(|d| d.min(old_write_pos))
        .unwrap_or(old_write_pos);
    if effective_end <= old_speech_start_pos {
        return None;
    }
    let snapshot = previous_buffer.snapshot_range(old_speech_start_pos, effective_end);
    if snapshot.samples.is_empty() {
        return None;
    }
    if snapshot.overrun {
        warn!(
            marker = "buffer_replaced_overrun",
            source = ?source.label,
            requested_start = old_speech_start_pos,
            actual_start = snapshot.start_pos,
            end = snapshot.end_pos,
            "previous buffer overran before pre-swap rescue could read it; \
             oldest unconsumed samples were lost"
        );
    }
    let mono = yapstack_common::audio::deinterleave_to_mono(&snapshot.samples, old_ch).into_owned();
    if mono.is_empty() {
        return None;
    }
    let duration = mono.len() as f32 / old_sr as f32;
    if duration < MIN_CHUNK_DURATION_SECS {
        return None;
    }

    let samples_since_start = snapshot.start_pos.saturating_sub(old_session_start_pos);
    let audio_offset_seconds =
        old_anchor + samples_since_start as f32 / (old_sr as f32 * old_ch as f32);

    let chunk_index = source.chunk_index;
    source.chunk_index = source.chunk_index.wrapping_add(1);
    let accumulated_text = std::mem::take(&mut source.accumulated_text);

    let sample_count = mono.len();
    let source_label = source.label;
    let prepared = PreparedChunk {
        samples: mono,
        sample_rate: old_sr,
        audio_offset_seconds,
        source_label,
        chunk_index,
        accumulated_text: accumulated_text.clone(),
        origin: JobOrigin::FinalFlush,
        drain_backlog_seconds: 0.0,
    };

    // Mark the source as in-flight via this rescue task. The normal
    // dispatch path's pending_outcomes drain will restore
    // `accumulated_text` from the rescue's outcome — same lifecycle as a
    // regular chunk.
    source.has_in_flight_task = true;
    source.force_drain = false;

    let task_ctx = ctx.clone();
    let task_counters = counters.clone();
    let task_prompt = prompt.clone();
    let handle = tokio::spawn(async move {
        run_chunk_task(prepared, task_ctx, task_counters, task_prompt).await
    });
    chunk_tasks.push(Box::pin(async move {
        match handle.await {
            Ok(outcome) => outcome,
            Err(e) => {
                error!(
                    "pre-swap rescue task panicked or was cancelled for source {:?}: {} \
                     — synthesizing outcome to free dispatch",
                    source_label, e
                );
                ChunkTaskOutcome {
                    source_label,
                    accumulated_text,
                    sidecar_dead: false,
                }
            }
        }
    }));
    Some((sample_count, duration))
}

/// Read `[ws.flush_positions.<source>_pos, prev_buf.write_pos)` from the old
/// ring and append it to the session WAV with resampling. Bridges the gap
/// that would otherwise appear in the persisted audio file when a buffer is
/// replaced between WAV flushes.
///
/// Scope: `MicOnly` and `SystemOnly` only. `Mixed` requires both sources'
/// old buffers at the swap instant for a faithful re-mix; only one source's
/// `previous_buffer` is available here, and falling back to "mix old-mic
/// with new-system" (or vice versa) would splice across the device boundary.
/// We log a warning and skip — the gap is bounded by the WAV-flush interval
/// (~100ms typical) and the WAV continues correctly from `new_pos_at_swap`.
///
/// Returns `Some((sample_count, duration_secs))` when audio was rescued,
/// `None` for empty extractions or unsupported capture modes.
fn rescue_pre_swap_wav_audio(
    ws: &mut SessionWavState,
    previous_buffer: &yapstack_audio::SharedAudioRingBuffer,
    source_label: AudioSourceLabel,
    // Earliest mic-buffer position inside the WAV rescue range where a
    // dictation window begins. The rescue is clamped to end here so
    // dictated audio doesn't appear in the persisted recording. `None`
    // means no dictation overlap or non-mic source. Always `None` for
    // System audio.
    dictation_truncate_at: Option<usize>,
    source_name: &str,
) -> Option<(usize, f32)> {
    let old_flush_pos = match source_label {
        AudioSourceLabel::Mic => ws.flush_positions.mic_pos,
        AudioSourceLabel::System => ws.flush_positions.system_pos,
    };
    match ws.source {
        CaptureSource::MicOnly if !matches!(source_label, AudioSourceLabel::Mic) => return None,
        CaptureSource::SystemOnly if !matches!(source_label, AudioSourceLabel::System) => {
            return None
        }
        CaptureSource::Mixed => {
            warn!(
                marker = "buffer_replaced_wav_gap_mixed",
                source = ?source_label,
                "Mixed-mode session WAV rescue is out of scope; pre-swap audio \
                 between last flush and swap is lost from the recorded file. \
                 The transcript rescue covers the same range so transcription \
                 continuity is preserved."
            );
            return None;
        }
        _ => {}
    }

    let old_write_pos = previous_buffer.samples_written();
    let effective_end = dictation_truncate_at
        .map(|d| d.min(old_write_pos))
        .unwrap_or(old_write_pos);
    if effective_end <= old_flush_pos {
        return None;
    }
    let snapshot = previous_buffer.snapshot_range(old_flush_pos, effective_end);
    if snapshot.samples.is_empty() {
        return None;
    }
    if snapshot.overrun {
        warn!(
            marker = "buffer_replaced_wav_overrun",
            source = ?source_label,
            requested_start = old_flush_pos,
            actual_start = snapshot.start_pos,
            "previous buffer overran before WAV rescue could read it; \
             oldest pre-swap WAV samples were lost"
        );
    }
    let mono =
        yapstack_common::audio::deinterleave_to_mono(&snapshot.samples, previous_buffer.channels())
            .into_owned();
    if mono.is_empty() {
        return None;
    }
    let prev_sr = previous_buffer.sample_rate();
    let to_write: std::borrow::Cow<[f32]> = if prev_sr == ws.wav_sample_rate {
        std::borrow::Cow::Borrowed(&mono)
    } else {
        match yapstack_common::audio::resample(&mono, prev_sr, ws.wav_sample_rate) {
            Ok(cow) => std::borrow::Cow::Owned(cow.into_owned()),
            Err(e) => {
                error!(
                    "WAV pre-swap rescue resample {}Hz → {}Hz failed, dropping {} samples: {}",
                    prev_sr,
                    ws.wav_sample_rate,
                    mono.len(),
                    e
                );
                return None;
            }
        }
    };
    let written = to_write.len();
    if let Err(e) = ws.writer.write_samples(&to_write) {
        error!(
            "session WAV pre-swap rescue write error ({} samples may be lost) on {}: {}",
            written, source_name, e
        );
        return None;
    }
    let duration = mono.len() as f32 / prev_sr as f32;
    Some((written, duration))
}

async fn live_transcription_loop(
    audio_state: AudioManagerState,
    ctx: TranscriptionContext,
    mut stop_rx: oneshot::Receiver<StopRequest>,
    stop_requested: Arc<AtomicBool>,
    mut restart_intent_rx: tokio::sync::mpsc::UnboundedReceiver<RestartIntent>,
    mut session_wav_state: Option<SessionWavState>,
    counters: Arc<StdMutex<SessionCounters>>,
) {
    // Live-loop-private prompt state. Backfill owns its own (see
    // `process_backfill`); they bridge via `ctx.bridged_prompt`.
    let prompt = Arc::new(StdMutex::new(PromptState {
        shared_prompt: String::new(),
        last_transcription_at: None,
    }));
    let source = ctx.config.source.clone().into();
    let check_mic = matches!(source, CaptureSource::MicOnly | CaptureSource::Mixed);
    let check_system = matches!(source, CaptureSource::SystemOnly | CaptureSource::Mixed);

    // VAD tuning resolved at session start (see start_live_transcription)
    // and shared via `ctx.engine_profile`. Re-bind locally so the rest of
    // the loop body can keep its existing `tuning.*` field accesses.
    let tuning = ctx.engine_profile.vad_tuning;

    let (mut sources, backfill_audio) = build_initial_sources_and_backfill(
        &audio_state,
        &ctx.config,
        check_mic,
        check_system,
        source,
        ctx.dictation_start_pos,
        ctx.session_offset_base_seconds,
    )
    .await;

    // Spawn concurrent backfill processing.
    //
    // Backfill is no longer cancelled on stop — its submitter task keeps
    // running after the live loop has exited and the stop path waits for
    // it to finish. The submitter submits each chunk to the scheduler at
    // `Backfill` priority and *awaits the scheduler response before
    // submitting the next chunk* (each chunk's prompt context depends on
    // the prior chunk's transcribed text, and segments emit in order as
    // results arrive). That means the wait for backfill to finish is a
    // wait for actual transcription, not a wait for an enqueue loop —
    // chunks not yet submitted by the submitter at abort time are not on
    // any durable queue and will be lost. The stop path's submitter-join
    // timeout (default 5 min) is the real ceiling on backfill drain;
    // `shutdown_and_return`'s timeout only governs whatever single chunk
    // is in-flight at the scheduler when the submitter exits. The abort
    // handle is the last-resort escape hatch if the submitter is genuinely
    // stuck (e.g. deadlocked Silero init).
    let backfill_done = Arc::new(AtomicBool::new(false));
    let backfill_handle = if !backfill_audio.is_empty() {
        // Namespace live chunk indices to avoid collision with backfill (0..N)
        for s in &mut sources {
            s.chunk_index = 10_000;
        }
        let backfill_ctx = ctx.clone();
        let backfill_done_clone = backfill_done.clone();
        let backfill_counters = counters.clone();
        let handle = tokio::spawn(process_backfill(
            backfill_ctx,
            backfill_audio,
            backfill_done_clone,
            tuning,
            backfill_counters,
        ));
        let abort_handle = handle.abort_handle();
        Some((handle, abort_handle))
    } else {
        // No backfill audio to process — either the user requested zero, the
        // ring buffer had no history, or this is a resume. Tell the frontend
        // immediately so the "backfill in progress" UI affordance clears
        // even when the user requested non-zero backfill but the buffer was
        // too short to honor it. Suppressed for dictation (see comment at
        // the other backfill-complete emit site).
        backfill_done.store(true, Ordering::Release);
        if matches!(ctx.config.source_kind, LiveSourceKind::Session) {
            let _ = ctx.app_handle.emit("backfill-complete", ());
        }
        None
    };

    let mut wav_flush_none_count: u32 = 0;
    let mut prompt_seeded_from_backfill = false;

    emit_status(
        &ctx,
        LiveTranscriptionPhase::Running,
        StatusProgress::zero(),
    );

    let poll_interval = tuning.poll_interval;
    let mut exited_fatal = false;
    let mut stop_request: Option<StopRequest> = None;

    // Silero VAD runs in-process (bundled V5 ONNX). Single session shared
    // across sources; per-source streaming state lives on each VAD state's
    // `silero` field. `Session` is `Send` but not `Sync`, so we hold it
    // `mut` locally and feed sources sequentially inside each poll.
    let mut silero = match SileroVad::new() {
        Ok(v) => v,
        Err(e) => {
            error!(
                "live transcription: failed to initialize Silero VAD — bailing out: {}",
                e
            );
            emit_status(&ctx, LiveTranscriptionPhase::Error, StatusProgress::zero());
            return;
        }
    };

    // In-flight chunk transcription tasks. `FuturesUnordered` so we can
    // wake the main loop the moment *any* task completes (via `.next()`
    // in the select!), rather than waiting for the next natural tick.
    // This cuts up to `poll_interval` of latency from the case where a
    // deferred `Chunk` action is waiting for a task to finish before it
    // can dispatch (e.g. a second utterance during the previous task's
    // transcribe). Outcomes drained here are applied at the top of the
    // tick below so VAD / dispatch see the updated per-source state.
    //
    // Each spawn is wrapped in an async block that traps `JoinError` and
    // synthesizes a fallback `ChunkTaskOutcome`. This guarantees the
    // invariant "every spawned task → exactly one outcome observed",
    // which keeps `has_in_flight_task` from leaking on panic/cancel and
    // would otherwise silently kill dispatch for that source forever.
    let mut chunk_tasks: futures_util::stream::FuturesUnordered<ChunkTaskFuture> =
        futures_util::stream::FuturesUnordered::new();
    // No parallel `AbortHandle` list: under the scheduler the worker is the
    // sole holder of the `Arc<TranscriptionClient>`, and chunk tasks just
    // submit jobs and await a oneshot. On stop the scheduler's
    // `shutdown_and_return` cancels still-pending jobs after its drain
    // window — no need to force-resolve in-flight chunks here.
    let mut pending_outcomes: Vec<ChunkTaskOutcome> = Vec::new();

    loop {
        use futures_util::stream::StreamExt;
        use tokio::time::sleep;
        // Wait for the earliest of: a natural tick, a chunk task finishing,
        // or a stop signal. We recreate `sleep(poll_interval)` each
        // iteration so the next natural tick resets to `poll_interval` from
        // now — the tick cadence slides forward after an early wake, which
        // is fine here because the VAD state machine uses wall-clock
        // durations (Instant::now()), not tick count.
        let stop_signal = tokio::select! {
            _ = sleep(poll_interval) => None,
            Some(outcome) = chunk_tasks.next(), if !chunk_tasks.is_empty() => {
                pending_outcomes.push(outcome);
                // Drain any other tasks that happen to be ready in this
                // same wakeup window without blocking.
                while let Some(Some(outcome)) = chunk_tasks.next().now_or_never() {
                    pending_outcomes.push(outcome);
                }
                None
            }
            // Device broker is asking us to fail over a Source. We
            // honor it by calling the existing `attempt_source_restart`
            // path (which resets `SourceVadState` cursor + Silero state
            // and updates the session-WAV flush position). The broker
            // has already debounced and `device_liveness`-gated, so we
            // don't re-check those here. Returning `None` signals "not
            // a stop", so the loop continues normally on the next tick.
            Some(intent) = restart_intent_rx.recv() => {
                let target_label = match intent {
                    RestartIntent::Mic => AudioSourceLabel::Mic,
                    RestartIntent::System => AudioSourceLabel::System,
                };
                if let Some(source) = sources.iter_mut().find(|s| s.label == target_label) {
                    let ws_borrow = session_wav_state.as_mut();
                    // Broker-driven restart: the OS just told us the
                    // default device changed. We *want* the new default,
                    // not the previously-bound one — probe the new
                    // default first and let `restart_mic` fall through
                    // to stored id/name only if the OS hasn't settled.
                    let report = attempt_source_restart(
                        source,
                        &audio_state,
                        &ctx,
                        "device-change",
                        yapstack_audio::manager::RestartTarget::FollowDefault,
                    )
                    .await;
                    if let Some(mut report) = report {
                        if report.outcome
                            == yapstack_audio::manager::RestartOutcome::BufferReplaced
                        {
                            let previous_buffer = report.previous_buffer.take();
                            handle_buffer_replacement(
                                source,
                                &audio_state,
                                &ctx,
                                &counters,
                                &prompt,
                                &mut chunk_tasks,
                                &mut pending_outcomes,
                                previous_buffer,
                                ws_borrow,
                                source_display_name(&source.label),
                            )
                            .await;
                        }
                    }
                }
                None
            }
            req = &mut stop_rx => req.ok(),
        };

        if let Some(req) = stop_signal {
            ctx.config
                .source_kind
                .set_scheduler_busy(&ctx.scheduler, true);
            stop_request = Some(req);
            break;
        }

        if let Some(req) =
            stop_if_requested(&stop_requested, &mut stop_rx, &audio_state, &ctx).await
        {
            stop_request = Some(req);
            break;
        }

        // Single lock: per-source audio extraction for Silero VAD +
        // WAV flush extraction. We pull raw mono samples (not just RMS)
        // for each source since the source's `silero.read_pos`, then feed
        // them through Silero outside the lock so the manager isn't held
        // while the ONNX session runs.
        struct SileroPollInput {
            label: AudioSourceLabel,
            samples: Option<(Vec<f32>, u32)>,
            new_pos: usize,
        }
        let (silero_inputs, wav_flush_data): (Vec<SileroPollInput>, _) = {
            let manager = audio_state.lock().await;
            let mut inputs: Vec<SileroPollInput> = Vec::with_capacity(sources.len());
            for s in &sources {
                let (samples, new_pos) =
                    extract_source_audio(&manager, &s.label, s.silero.read_pos);
                inputs.push(SileroPollInput {
                    label: s.label,
                    samples,
                    new_pos,
                });
            }
            let flush = session_wav_state.as_ref().and_then(|ws| {
                let old_mic_pos = ws.flush_positions.mic_pos as u64;
                let mic_channels = manager.mic_buffer().map(|b| b.channels()).unwrap_or(1) as usize;
                let mut extraction = manager.extract_since(
                    &ws.flush_positions,
                    ws.source,
                    ws.mix_config.as_ref(),
                )?;
                mute_dictation_window_in_wav_extraction(
                    &mut extraction.0,
                    mic_channels,
                    old_mic_pos,
                    &ctx,
                    ws.source,
                );
                Some(extraction)
            });
            (inputs, flush)
        };

        if let Some(req) =
            stop_if_requested(&stop_requested, &mut stop_rx, &audio_state, &ctx).await
        {
            stop_request = Some(req);
            break;
        }

        // Feed Silero outside the audio lock and collect *every* frame
        // probability per source, in emission order. A single poll batch
        // can span an entire short utterance; the VAD state machine needs
        // to see the intermediate loud frames, not just the trailing
        // silence frame that follows them.
        //
        // Sticky behavior: when no new samples arrived (empty buffer
        // snapshot), we carry the source's `last_probability` forward so
        // the state machine keeps making decisions on the most recent
        // signal rather than toggling to `None`.
        let mut per_source_probs: Vec<(AudioSourceLabel, Vec<f32>)> =
            Vec::with_capacity(silero_inputs.len());
        for input in silero_inputs {
            if let Some(s) = sources.iter_mut().find(|s| s.label == input.label) {
                s.silero.read_pos = input.new_pos;
                let probs = match input.samples {
                    Some((mono, source_sr)) => {
                        silero.score_stream(&mut s.silero.stream, &mono, source_sr)
                    }
                    None => Vec::new(),
                };
                if let Some(&last) = probs.last() {
                    s.silero.last_probability = Some(last);
                }
                per_source_probs.push((input.label, probs));
            }
        }

        if let Some(req) =
            stop_if_requested(&stop_requested, &mut stop_rx, &audio_state, &ctx).await
        {
            stop_request = Some(req);
            break;
        }

        if let Some((samples, sr, new_pos)) = wav_flush_data {
            wav_flush_none_count = 0;
            if let Some(ref mut ws) = session_wav_state {
                if matches!(
                    write_session_wav_samples(ws, &samples, sr, new_pos),
                    WavWriteOutcome::ResampleFailed
                ) {
                    continue;
                }
            }
        } else if session_wav_state.is_some() {
            handle_empty_wav_flush(
                &mut wav_flush_none_count,
                session_wav_state.as_ref(),
                &audio_state,
                &ctx,
                &backfill_done,
            )
            .await;
        }

        if let Some(req) =
            stop_if_requested(&stop_requested, &mut stop_rx, &audio_state, &ctx).await
        {
            stop_request = Some(req);
            break;
        }

        // Stream health watchdog: check for cpal error flags and write_pos stalls.
        // Triggers auto-restart if a stream has died silently.
        // restart_mic() tries the previously stored device first, then falls back to
        // the provided name (None = system default).
        let mixed_fail_fast = check_stream_health(
            &mut sources,
            &audio_state,
            &ctx,
            &counters,
            &prompt,
            &mut chunk_tasks,
            &mut pending_outcomes,
            session_wav_state.as_mut(),
            matches!(source, CaptureSource::Mixed),
        )
        .await;

        // Mixed mid-capture fail-fast: stop both Sources and exit. The
        // per-Source `restart_abandoned` toast was already emitted; the
        // session naturally winds down through the standard cleanup
        // path (drain in-flight chunks, finalize WAV, etc.).
        if mixed_fail_fast {
            warn!("Mixed capture: terminal restart failure on a Source — ending session");
            let mut manager = audio_state.lock().await;
            let _ = manager.stop_all();
            drop(manager);
            break;
        }

        if !prompt_seeded_from_backfill
            && seed_prompt_from_backfill(&ctx, &prompt, &mut sources).await
        {
            prompt_seeded_from_backfill = true;
        }

        // First: drain any completed chunk tasks (both the ones collected
        // by the `select!` wake above and any that finished during the
        // subsequent awaits) and apply their outcomes so their sources
        // become eligible for VAD polling again. We do this *before*
        // polling VAD so a just-completed task's source gets a fresh
        // poll_vad call (updated is_speaking / silence_since) this tick,
        // not next tick.
        let mut fatal = false;
        // Drain any additional outcomes that became ready while the tick
        // waited on other awaits (audio_state lock, WAV flush, etc).
        while let Some(Some(outcome)) = chunk_tasks.next().now_or_never() {
            pending_outcomes.push(outcome);
        }
        let drained = std::mem::take(&mut pending_outcomes);
        for outcome in drained {
            for source in sources.iter_mut() {
                if source.label == outcome.source_label {
                    source.has_in_flight_task = false;
                    // Restore accumulated_text from the task. Safe because
                    // has_in_flight_task gated all reads of this field
                    // while the task was running.
                    source.accumulated_text = outcome.accumulated_text;
                    break;
                }
            }
            if outcome.sidecar_dead {
                fatal = true;
            }
        }

        if let Some(req) =
            stop_if_requested(&stop_requested, &mut stop_rx, &audio_state, &ctx).await
        {
            stop_request = Some(req);
            break;
        }

        // Poll VAD for *every* source, including those with an in-flight
        // task. The state machine needs to keep tracking is_speaking /
        // silence_since so a second utterance that starts and ends during
        // the in-flight window is captured: onset backdates
        // `speech_start_pos` (via pre_roll), silence_since fires a pending
        // Chunk action, and on the tick *after* the task lands we dispatch
        // that chunk with the full accumulated range.
        //
        // We feed *every* Silero probability from this batch through
        // `poll_vad` in order — intra-poll speech events would otherwise
        // be lost when only the trailing probability (likely silence) is
        // observed. If any frame produces a Chunk / ForceChunk action, we
        // remember it for the dispatch step below; at most one chunk can
        // actually fire per source per poll (has_in_flight_task gate), so
        // keeping the strongest observed action is the right summary.
        //
        // Dispatch and idle-cursor advance are still gated on
        // `has_in_flight_task`: we can't dispatch a second task for the
        // same source, and `advance_idle_cursor` would drop
        // `speech_start_pos` past audio that belongs to the next chunk.
        let actions: Vec<(VadAction, JobOrigin)> = sources
            .iter_mut()
            .map(|source| {
                let probs = per_source_probs
                    .iter()
                    .find(|(l, _)| *l == source.label)
                    .map(|(_, p)| p.as_slice())
                    .unwrap_or(&[]);

                let mut best_action = VadAction::None;
                if source.force_drain {
                    best_action = VadAction::ForceChunk;
                } else if probs.is_empty() {
                    // Sticky fallback: no new frames in this batch. Use
                    // last_probability so the state machine keeps running
                    // against the most recent signal rather than freezing.
                    let action = poll_vad(source, source.silero.last_probability, &tuning);
                    if matches!(action, VadAction::Chunk | VadAction::ForceChunk) {
                        best_action = action;
                    }
                } else {
                    for &prob in probs {
                        let action = poll_vad(source, Some(prob), &tuning);
                        match action {
                            VadAction::ForceChunk => best_action = VadAction::ForceChunk,
                            VadAction::Chunk if !matches!(best_action, VadAction::ForceChunk) => {
                                best_action = VadAction::Chunk;
                            }
                            _ => {}
                        }
                    }
                }

                // Tag mic chunks `Dictation` for dictation runtimes so they
                // jump the scheduler queue past session live chunks. System
                // chunks aren't possible for dictation (config validation
                // rejects non-mic dictation at the boundary), but we route
                // by source label defensively.
                let job_origin = match (ctx.config.source_kind, source.label) {
                    (LiveSourceKind::Dictation, AudioSourceLabel::Mic) => JobOrigin::Dictation,
                    _ => JobOrigin::Live,
                };
                (best_action, job_origin)
            })
            .collect();

        // Mic-ownership: drive session mic state from the explicit
        // `[start, end)` windows in `dictation_owns_mic`. Two-step:
        //   (a) Consume any *closed* windows that landed since the
        //       previous tick. Each one: pre-flush the user's pending
        //       session speech up to `window.start` (so the partial
        //       word in-progress at hotkey press isn't lost), then
        //       reset all VAD/cursor state to `window.end`. Closed
        //       windows are durable — a fast dictation that opens
        //       AND closes between two session polls still appears
        //       in `closed` and is processed here, so the boolean-
        //       flag race is impossible to miss.
        //   (b) Pre-flush for the *open* window the first time we
        //       observe its start (avoids re-flushing every tick
        //       while the same window stays open).
        // Mic dispatch for this tick is suspended whenever an open
        // window exists OR we processed any closed windows (the
        // pre-tick VAD-poll output reflects pre-window state and is
        // stale after reset).
        let mut suspend_mic_dispatch = false;
        if matches!(ctx.config.source_kind, LiveSourceKind::Session) {
            let snapshot = ctx.dictation_owns_mic.snapshot();
            let mic_idx = sources
                .iter()
                .position(|s| matches!(s.label, AudioSourceLabel::Mic));
            if let Some(i) = mic_idx {
                // (a) Consume any closed windows past this source's
                //     `dictation_consumed_through`. Position-based instead
                //     of index-based so the closed list can be pruned
                //     (front entries dropped when both consumers have
                //     moved past them) without invalidating progress here.
                //     Cloning into a local Vec since `snapshot` owns the
                //     deque and we mutate `sources[i]` in the loop body.
                let new_windows: Vec<MicWindow> = snapshot
                    .closed
                    .iter()
                    .copied()
                    .filter(|w| w.end > sources[i].dictation_consumed_through)
                    .collect();
                for window in new_windows {
                    let window_start_us = window.start as usize;
                    let window_end_us = window.end as usize;
                    if sources[i].is_speaking
                        && !sources[i].has_in_flight_task
                        && window_start_us > sources[i].speech_start_pos
                    {
                        dispatch_pre_window_flush(
                            &mut sources[i],
                            &audio_state,
                            &ctx,
                            &counters,
                            &prompt,
                            &mut chunk_tasks,
                            tuning.max_chunk_duration,
                            window_start_us,
                        )
                        .await;
                    }
                    // Any VAD progress accumulated during the dictation
                    // window is dictation audio and must not become
                    // session transcript — rewind past `window.end`.
                    sources[i].reset_vad_past(window_end_us);
                    sources[i].dictation_consumed_through = window.end;
                    // If the consumed window matches a previously pre-flushed
                    // open window's start, drop that marker.
                    if sources[i].flushed_open_at == Some(window.start) {
                        sources[i].flushed_open_at = None;
                    }
                    suspend_mic_dispatch = true;
                }

                // (b) Pre-flush for the open window, once per window.
                if let Some(open_start) = snapshot.open {
                    if sources[i].flushed_open_at != Some(open_start) {
                        let open_start_us = open_start as usize;
                        if sources[i].is_speaking
                            && !sources[i].has_in_flight_task
                            && open_start_us > sources[i].speech_start_pos
                        {
                            dispatch_pre_window_flush(
                                &mut sources[i],
                                &audio_state,
                                &ctx,
                                &counters,
                                &prompt,
                                &mut chunk_tasks,
                                tuning.max_chunk_duration,
                                open_start_us,
                            )
                            .await;
                        }
                        sources[i].flushed_open_at = Some(open_start);
                    }
                    suspend_mic_dispatch = true;
                }
            }
        }

        // Dispatch new chunk tasks (fire and forget). Per-source state is
        // advanced optimistically inside prepare_chunk_dispatch; the spawned
        // task submits to the scheduler and emits segments. The main loop
        // continues polling other sources next tick.
        for (i, (action, origin)) in actions.iter().enumerate() {
            // Mic-ownership suspension: skip session-mic dispatch when an
            // open dictation window exists OR when this tick consumed any
            // closed windows (in which case the pre-tick VAD output for
            // mic source is stale relative to the post-reset state).
            // System audio is never affected.
            if matches!(ctx.config.source_kind, LiveSourceKind::Session)
                && matches!(sources[i].label, AudioSourceLabel::Mic)
                && suspend_mic_dispatch
            {
                continue;
            }
            let in_flight = sources[i].has_in_flight_task;
            match action {
                VadAction::Chunk | VadAction::ForceChunk => {
                    if in_flight {
                        // Can't dispatch while the previous task is running.
                        // VAD state stays as poll_vad set it (is_speaking
                        // cleared, silence_since armed), so the next tick
                        // after the task lands will re-evaluate and fire a
                        // fresh Chunk from the accumulated speech_start_pos
                        // through the current write_pos.
                        continue;
                    }
                    let is_force = matches!(action, VadAction::ForceChunk);
                    let prepared = prepare_chunk_dispatch(
                        &mut sources[i],
                        &audio_state,
                        is_force,
                        *origin,
                        tuning.max_chunk_duration,
                    )
                    .await;
                    if let Some(prepared) = prepared {
                        record_live_drain_backlog(
                            &counters,
                            prepared.origin,
                            prepared.drain_backlog_seconds,
                        );
                        let task_ctx = ctx.clone();
                        let task_counters = counters.clone();
                        let task_prompt = prompt.clone();
                        let source_label = prepared.source_label;
                        let fallback_text = prepared.accumulated_text.clone();
                        let handle = tokio::spawn(async move {
                            run_chunk_task(prepared, task_ctx, task_counters, task_prompt).await
                        });
                        chunk_tasks.push(Box::pin(async move {
                            match handle.await {
                                Ok(outcome) => outcome,
                                Err(e) => {
                                    error!(
                                        "chunk task panicked or was cancelled for source {:?}: {} \
                                         — synthesizing outcome to free dispatch",
                                        source_label, e
                                    );
                                    ChunkTaskOutcome {
                                        source_label,
                                        accumulated_text: fallback_text,
                                        sidecar_dead: false,
                                    }
                                }
                            }
                        }));
                    }
                }
                VadAction::None => {
                    // Only advance the idle cursor when there's no in-flight
                    // task for this source AND we're confident no speech is
                    // pending. `has_in_flight_task` means `speech_start_pos`
                    // must stay frozen at the task's new_pos so any utterance
                    // that happened during the in-flight window still gets
                    // covered by the next chunk dispatch.
                    if !in_flight && !sources[i].is_speaking {
                        advance_idle_cursor(&mut sources[i], &audio_state).await;
                    }
                }
            }
        }

        if fatal {
            emit_fatal_sidecar_error(&ctx, &counters);
            exited_fatal = true;
            break;
        }

        run_prompt_decay(&ctx, &prompt, &mut sources);

        update_scheduler_live_busy(&ctx, &sources, chunk_tasks.len(), false);

        // Prune closed dictation windows that every consumer has already
        // moved past. Keeps the closed list bounded by the number of
        // in-flight-or-recently-finished dictations regardless of session
        // length — without this, the list grows once per dictation for
        // the entire session and is cloned on every poll. Only relevant
        // for Session runtimes (the dictation runtime doesn't read the
        // closed list and isn't a consumer here).
        if matches!(ctx.config.source_kind, LiveSourceKind::Session) {
            if let Some(mic) = sources
                .iter()
                .find(|s| matches!(s.label, AudioSourceLabel::Mic))
            {
                // Min consumer position is the slowest of:
                //   - the mic VAD's `dictation_consumed_through`
                //   - the session-WAV writer's `flush_positions.mic_pos`
                // A window with `end <= min_pos` is past every consumer.
                let mic_consumed = mic.dictation_consumed_through;
                let wav_consumed = session_wav_state
                    .as_ref()
                    .map(|ws| ws.flush_positions.mic_pos as u64)
                    .unwrap_or(u64::MAX);
                let min_pos = mic_consumed.min(wav_consumed);
                ctx.dictation_owns_mic.prune_before(min_pos);
            }
        }
    }

    if stop_request.is_some() {
        tokio::time::sleep(Duration::from_secs_f32(STOP_TAIL_GRACE_SECS)).await;
    }

    let final_pending_chunks = if let Some(req) = stop_request.as_ref() {
        // Skip mic-side final flush when this is a Session whose pending
        // mic range overlaps any dictation window — open or closed. The
        // mic source's `speech_start_pos..req.positions.mic_pos` range
        // is what the final flush would copy; if any dictation window
        // overlaps that range, those samples are dictation audio and
        // must not become session segments. Driven entirely off the
        // snapshot, so a fast dictation that opened-and-closed during
        // the stop tail still appears as a closed window and is
        // detected here even if the live loop never observed the open
        // state mid-flight.
        let skip_mic = matches!(ctx.config.source_kind, LiveSourceKind::Session) && {
            let snapshot = ctx.dictation_owns_mic.snapshot();
            let mic_source = sources
                .iter()
                .find(|s| matches!(s.label, AudioSourceLabel::Mic));
            match mic_source {
                Some(s) => {
                    snapshot.overlaps(s.speech_start_pos as u64, req.positions.mic_pos as u64)
                }
                None => false,
            }
        };
        copy_final_pending_chunks_until(
            &sources,
            &audio_state,
            tuning.max_chunk_duration,
            &req.positions,
            skip_mic,
        )
        .await
    } else {
        Vec::new()
    };

    if let (Some(ref mut ws), Some(req)) = (session_wav_state.as_mut(), stop_request.as_ref()) {
        flush_session_wav_to_limit(ws, &audio_state, Some(&req.positions), &ctx).await;
    }

    drain_in_flight_chunks(&mut chunk_tasks, &mut sources).await;

    if stop_request.is_some() {
        dispatch_copied_final_chunks(&mut sources, final_pending_chunks, &ctx, &counters, &prompt)
            .await;
    } else {
        dispatch_final_pending_chunks(
            &mut sources,
            &audio_state,
            &ctx,
            &counters,
            &prompt,
            tuning.max_chunk_duration,
        )
        .await;
        if let Some(ref mut ws) = session_wav_state {
            flush_session_wav_to_limit(ws, &audio_state, None, &ctx).await;
        }
    }

    // Clear this loop's busy bits at finalize so it stops gating backfill.
    ctx.config
        .source_kind
        .set_scheduler_busy(&ctx.scheduler, false);

    if let Some(ws) = session_wav_state {
        finalize_session_wav(ws, &ctx).await;
    }

    // Wait for concurrent backfill to finish before emitting Stopped.
    //
    // Unlike the previous design, we never proactively cancel the backfill
    // submitter on stop. The submitter walks its in-memory chunk list and
    // awaits each scheduler response in turn; the scheduler prioritizes
    // `FinalFlush` and `Live` work over `Backfill`, so any closing-words
    // chunks queued at stop outrank remaining backfill and drain quickly.
    // The submitter then continues working through whatever backfill chunks
    // remain.
    //
    // This wait *is* the real ceiling on backfill drain time, because the
    // submitter awaits each chunk's response before submitting the next.
    // `shutdown_and_return` below only governs whatever single chunk
    // (FinalFlush, Live, or Backfill) is in-flight at the scheduler when
    // the submitter exits — chunks not yet submitted by the submitter at
    // abort time are not on any durable queue and will be lost. The 5-min
    // submitter-join timeout is generous enough that a normal backfill
    // window completes; the abort handle is the last-resort escape hatch
    // for a genuinely stuck submitter.
    if let Some((handle, abort_handle)) = backfill_handle {
        match tokio::time::timeout(Duration::from_secs(DEFAULT_SHUTDOWN_TIMEOUT_SECS), handle).await
        {
            Ok(_) => {}
            Err(_) => {
                warn!(
                    "backfill submitter did not exit within {}s — aborting; \
                     scheduler shutdown will cancel any still-pending chunks",
                    DEFAULT_SHUTDOWN_TIMEOUT_SECS
                );
                abort_handle.abort();
            }
        }
    }

    let (
        final_chunks,
        final_audio_seconds,
        final_backlog_chunks,
        final_backlog_seconds,
        final_lag,
        final_total_wall_ms,
    ) = {
        let s = counters.lock().expect("counters mutex poisoned");
        let lag = s.latest_completed_audio_offset_seconds.map(|chunk_end| {
            let session_time_now =
                ctx.session_offset_base_seconds + ctx.session_start_instant.elapsed().as_secs_f32();
            (session_time_now - chunk_end).max(0.0)
        });
        (
            s.total_chunks,
            s.total_audio_seconds,
            s.live_drain_backlog_chunks,
            s.live_drain_backlog_seconds,
            lag,
            s.total_wall_ms,
        )
    };

    // Only emit Stopped if we didn't already emit Error (avoids duplicate finalization)
    if !exited_fatal {
        emit_status(
            &ctx,
            LiveTranscriptionPhase::Stopped,
            StatusProgress {
                chunks: final_chunks,
                audio_secs: final_audio_seconds,
                lag_seconds: final_lag,
                live_drain_backlog_chunks: final_backlog_chunks,
                live_drain_backlog_seconds: final_backlog_seconds,
            },
        );
    }

    // Session pressure summary — one structured line per session so log
    // captures from a stalled run carry the aggregate signal even when the
    // per-chunk lines are too verbose to scan.
    let mean_rtfx = if final_total_wall_ms > 0 && final_audio_seconds > 0.0 {
        Some(final_audio_seconds / (final_total_wall_ms as f32 / 1000.0))
    } else {
        None
    };
    info!(
        marker = "live_session_summary",
        engine = ctx.engine_profile.engine_name,
        session_id = ?ctx.config.session_id,
        chunks = final_chunks,
        audio_secs = final_audio_seconds,
        wall_ms = final_total_wall_ms,
        mean_rtfx = ?mean_rtfx,
        live_drain_backlog_chunks = final_backlog_chunks,
        live_drain_backlog_seconds = final_backlog_seconds,
        final_lag_secs = ?final_lag,
        exited_fatal = exited_fatal,
        "live transcription session ended"
    );
}

/// Build the per-source VAD state for the live loop and, if `backfill_seconds`
/// is non-zero, extract the matching pre-session audio and reset each source's
/// cursors / Silero stream forward to the current write position. Reading
/// backfill before the live loop starts guarantees a single coherent snapshot
/// — the loop then begins polling at the post-backfill cursor. Returns the
/// initialized sources alongside the backfill audio queued for concurrent
/// processing.
async fn build_initial_sources_and_backfill(
    audio_state: &AudioManagerState,
    config: &LiveTranscriptionConfig,
    check_mic: bool,
    check_system: bool,
    source: CaptureSource,
    dictation_start_pos: Option<u64>,
    // Initial anchor for every source's `audio_offset_anchor_seconds`. Equal
    // to `ctx.session_offset_base_seconds` at loop entry (the "prior parts
    // duration" baked into the resume contract). Sources mutate their own
    // anchor on `BufferReplaced`; this argument only seeds the value.
    session_offset_base_seconds: f32,
) -> (Vec<SourceVadState>, Vec<(Vec<f32>, u32, AudioSourceLabel)>) {
    let manager = audio_state.lock().await;
    let positions = manager.buffer_positions();

    let has_backfill = config.backfill_seconds > 0.0;
    let mut sources: Vec<SourceVadState> = Vec::with_capacity(2);
    let mut backfill: Vec<(Vec<f32>, u32, AudioSourceLabel)> = Vec::new();

    if check_mic {
        let (initial_pos, session_start, sr, ch) = if let Some(buf) = manager.mic_buffer() {
            let sr = buf.sample_rate();
            let ch = buf.channels();
            // Dictation runtimes initialize at their `dictation_start_pos`
            // (captured when the runtime opened its mic-ownership window)
            // rather than the current write position. Without this, the
            // audio captured between hotkey press and live-loop spawn is
            // dropped on the floor: the session correctly skips it (it's
            // inside an open dictation window) but dictation never reads
            // from before the spawn, so neither pipeline ever sees it.
            // For sessions, fall through to the normal backfill / current-
            // pos selection — the dictation_start_pos arg is None.
            if let Some(start) = dictation_start_pos {
                let clamped = (start as usize).min(positions.mic_pos);
                (clamped, clamped, sr, ch)
            } else if has_backfill {
                let raw = (config.backfill_seconds * sr as f32 * ch as f32) as usize;
                // Round down to frame boundary for correct deinterleaving.
                let rewind = raw - (raw % ch as usize);
                let rewound = positions.mic_pos.saturating_sub(rewind);
                (rewound, rewound, sr, ch)
            } else {
                (positions.mic_pos, positions.mic_pos, sr, ch)
            }
        } else {
            (positions.mic_pos, positions.mic_pos, 48000, 1)
        };
        sources.push(SourceVadState::new(
            AudioSourceLabel::Mic,
            initial_pos,
            session_start,
            sr,
            ch,
            session_offset_base_seconds,
        ));
    }
    if check_system {
        if let Some(buf) = manager.system_buffer() {
            let sr = buf.sample_rate();
            let ch = buf.channels();
            let (initial_pos, session_start) = if has_backfill {
                let raw = (config.backfill_seconds * sr as f32 * ch as f32) as usize;
                let rewind = raw - (raw % ch as usize);
                let rewound = positions.system_pos.saturating_sub(rewind);
                (rewound, rewound)
            } else {
                (positions.system_pos, positions.system_pos)
            };
            sources.push(SourceVadState::new(
                AudioSourceLabel::System,
                initial_pos,
                session_start,
                sr,
                ch,
                session_offset_base_seconds,
            ));
        } else if matches!(source, CaptureSource::Mixed) {
            warn!("mixed mode: system buffer unavailable, running mic-only");
        }
    }

    if has_backfill {
        info!(
            "live transcription: extracted backfill audio ({:.1}s) for concurrent processing",
            config.backfill_seconds
        );
        for s in sources.iter_mut() {
            let (audio, current) = extract_source_audio(&manager, &s.label, s.cursor);
            if let Some((samples, sr)) = audio {
                info!(
                    "backfill extract: source={:?} cursor={} write_pos={} samples={} sr={} duration={:.2}s",
                    s.label, s.cursor, current, samples.len(), sr, samples.len() as f32 / sr as f32
                );
                backfill.push((samples, sr, s.label));
            } else {
                warn!(
                    "backfill: no audio available for source={:?} (cursor={}, write_pos={})",
                    s.label, s.cursor, current
                );
            }
            // Reset the full per-source live VAD state to `current` so the
            // live loop starts from the post-backfill write position. Without
            // this, Silero would replay every backfill sample through the
            // *live* detector on the first few polls, duplicating or delaying
            // speech that backfill already emitted. We also reset the
            // recurrent stream state (LSTM memory was initialized at session
            // start against pre-backfill audio context) and clear the sticky
            // last_probability.
            s.cursor = current;
            s.speech_start_pos = current;
            s.earliest_next_chunk_pos = current;
            s.silero.read_pos = current;
            s.silero.reset();
        }
    }

    (sources, backfill)
}

/// Emit a final `Error`-phase status event after the sidecar has been
/// declared unrecoverable. Reads the running totals out of the shared
/// accumulator so the UI sees the same chunk/audio counts it would on a
/// normal stop.
fn emit_fatal_sidecar_error(ctx: &TranscriptionContext, counters: &Arc<StdMutex<SessionCounters>>) {
    error!("live transcription: sidecar died and could not be restarted — stopping");
    let (chunks, audio_secs, backlog_chunks, backlog_seconds, lag_seconds) = {
        let s = counters.lock().expect("counters mutex poisoned");
        let lag = s.latest_completed_audio_offset_seconds.map(|chunk_end| {
            let session_time_now =
                ctx.session_offset_base_seconds + ctx.session_start_instant.elapsed().as_secs_f32();
            (session_time_now - chunk_end).max(0.0)
        });
        (
            s.total_chunks,
            s.total_audio_seconds,
            s.live_drain_backlog_chunks,
            s.live_drain_backlog_seconds,
            lag,
        )
    };
    let _ = ctx.app_handle.emit(
        "live-transcription-status",
        LiveTranscriptionStatus {
            phase: LiveTranscriptionPhase::Error,
            chunks_processed: chunks,
            total_audio_seconds: audio_secs,
            error_message: Some(
                "Transcription engine stopped unexpectedly and could not be restarted".to_string(),
            ),
            session_id: ctx.config.session_id.clone(),
            effective_start_epoch_ms: None,
            lag_seconds,
            live_drain_backlog_chunks: backlog_chunks,
            live_drain_backlog_seconds: backlog_seconds,
            source_kind: ctx.config.source_kind,
        },
    );
}

/// Per-tick prompt decay check: clear `shared_prompt` and every source's
/// `accumulated_text` once `prompt_decay_silence_seconds` have elapsed since
/// the last successful transcription. Keeps stale context from seeding
/// hallucinations after a long pause.
fn run_prompt_decay(
    ctx: &TranscriptionContext,
    prompt: &Arc<StdMutex<PromptState>>,
    sources: &mut [SourceVadState],
) {
    let prompt_decay_secs = ctx.config.prompt_decay_silence_seconds.unwrap_or(5.0);
    // Hold the lock across read/check/clear/timestamp so the decision is
    // atomic w.r.t. concurrent chunk tasks. Releasing between steps would
    // let a freshly-completed chunk update `shared_prompt` /
    // `last_transcription_at` mid-decay, either clearing fresh prompt text
    // or clobbering a fresh timestamp with `None`.
    let elapsed = {
        let mut p = prompt.lock().expect("prompt mutex poisoned");
        let last_at = p.last_transcription_at;
        let decayed = check_prompt_decay(sources, &mut p.shared_prompt, prompt_decay_secs, last_at);
        if decayed {
            p.last_transcription_at = None;
            Some(last_at.map(|t| t.elapsed().as_secs_f32()).unwrap_or(0.0))
        } else {
            None
        }
    };
    if let Some(secs) = elapsed {
        info!(
            "prompt decay: cleared shared_prompt ({:.1}s since last transcription)",
            secs
        );
    }
}

/// Drain still-running chunk tasks at session stop so their segments dispatch
/// before finalization. The frontend's stop finalizer waits on
/// `segmentQueueTail`; that only helps once each task has actually emitted.
///
/// Under the scheduler, chunk tasks just submit a `JobRequest` and await a
/// oneshot — they don't hold an `Arc<TranscriptionClient>` clone, so we no
/// longer need an abort phase to reclaim shared state. Tasks at `FinalFlush`
/// priority complete first; any `Live`/`Backfill` chunks queued at stop are
/// either drained by the scheduler's worker or cancelled via
/// `shutdown_and_return` after the bounded shutdown timeout.
async fn drain_in_flight_chunks(
    chunk_tasks: &mut futures_util::stream::FuturesUnordered<ChunkTaskFuture>,
    sources: &mut [SourceVadState],
) {
    if chunk_tasks.is_empty() {
        return;
    }
    debug!(
        "live transcription stop: draining {} in-flight chunk tasks",
        chunk_tasks.len()
    );
    use futures_util::stream::StreamExt;

    let mut drained_outcomes: Vec<ChunkTaskOutcome> = Vec::new();
    let drain = async {
        while let Some(outcome) = chunk_tasks.next().await {
            if outcome.sidecar_dead {
                warn!("drained chunk task reported sidecar dead");
            }
            drained_outcomes.push(outcome);
        }
    };
    if tokio::time::timeout(Duration::from_secs(15), drain)
        .await
        .is_err()
    {
        warn!(
            "live transcription stop: chunk task drain exceeded 15s; \
             scheduler shutdown will cancel any still-pending chunks"
        );
    }

    for outcome in drained_outcomes {
        for source in sources.iter_mut() {
            if source.label == outcome.source_label {
                source.has_in_flight_task = false;
                source.accumulated_text = outcome.accumulated_text;
                break;
            }
        }
    }
}

struct FinalPendingChunk {
    samples: Vec<f32>,
    sample_rate: u32,
    audio_offset_seconds: f32,
    source_label: AudioSourceLabel,
}

async fn copy_final_pending_chunks_until(
    sources: &[SourceVadState],
    audio_state: &AudioManagerState,
    max_chunk_duration: Duration,
    stop_positions: &BufferPositions,
    skip_mic: bool,
) -> Vec<FinalPendingChunk> {
    let mut pending = Vec::new();
    for source in sources {
        if skip_mic && matches!(source.label, AudioSourceLabel::Mic) {
            // Final-flush mic guard: when a session is stopped while
            // dictation owns the mic, the session's pending mic range
            // (`speech_start_pos..stop_positions.mic_pos`) points into the
            // dictation window. Flushing it would emit dictated audio as a
            // session segment. The session was suspended on mic; there is
            // no legitimate pending mic content to flush.
            continue;
        }
        let limit = match source.label {
            AudioSourceLabel::Mic => stop_positions.mic_pos,
            AudioSourceLabel::System => stop_positions.system_pos,
        };
        let mut from = source.speech_start_pos;
        let max_raw = max_raw_samples_for_duration(
            max_chunk_duration,
            source.source_sample_rate,
            source.source_channels,
        );

        while from < limit {
            let until = from.saturating_add(max_raw).min(limit);
            let extraction = {
                let manager = audio_state.lock().await;
                extract_source_audio_until(&manager, &source.label, from, until)
            };
            let Some(extraction) = extraction else {
                break;
            };
            if extraction.overrun {
                warn!(
                    marker = "live_stop_ring_overrun",
                    source = ?source.label,
                    requested_start = from,
                    actual_start = extraction.start_pos,
                    stop_pos = limit,
                    "ring buffer overrun while copying stop-bounded final chunk"
                );
            }
            let duration = extraction.samples.len() as f32 / extraction.sample_rate as f32;
            if duration >= MIN_CHUNK_DURATION_SECS {
                let samples_since_start = extraction
                    .start_pos
                    .saturating_sub(source.session_start_pos);
                // Per-source anchor (not the session-wide offset_base) — see
                // the rationale at the main offset-formula site in
                // `prepare_chunk_dispatch_inner`. Without this, the final
                // pre-stop flush on a session that has experienced any
                // buffer replacement would stamp this chunk with a
                // pre-replacement offset, conflicting with everything
                // dispatched since the swap.
                let audio_offset_seconds = source.audio_offset_anchor_seconds
                    + samples_since_start as f32
                        / (source.source_sample_rate as f32 * source.source_channels as f32);
                pending.push(FinalPendingChunk {
                    samples: extraction.samples,
                    sample_rate: extraction.sample_rate,
                    audio_offset_seconds,
                    source_label: source.label,
                });
            }
            if extraction.end_pos <= from {
                break;
            }
            from = extraction.end_pos;
        }
    }
    pending
}

async fn dispatch_copied_final_chunks(
    sources: &mut [SourceVadState],
    chunks: Vec<FinalPendingChunk>,
    ctx: &TranscriptionContext,
    counters: &Arc<StdMutex<SessionCounters>>,
    prompt: &Arc<StdMutex<PromptState>>,
) {
    for chunk in chunks {
        let Some(source) = sources.iter_mut().find(|s| s.label == chunk.source_label) else {
            continue;
        };
        let chunk_index = source.chunk_index;
        source.chunk_index = source.chunk_index.wrapping_add(1);
        let accumulated_text = std::mem::take(&mut source.accumulated_text);
        let prepared = PreparedChunk {
            samples: chunk.samples,
            sample_rate: chunk.sample_rate,
            audio_offset_seconds: chunk.audio_offset_seconds,
            source_label: chunk.source_label,
            chunk_index,
            accumulated_text,
            origin: JobOrigin::FinalFlush,
            drain_backlog_seconds: 0.0,
        };
        let outcome = run_chunk_task(prepared, ctx.clone(), counters.clone(), prompt.clone()).await;
        source.accumulated_text = outcome.accumulated_text;
        source.has_in_flight_task = false;
        source.force_drain = false;
        if outcome.sidecar_dead {
            warn!("final copied chunk reported sidecar dead");
            break;
        }
    }
}

/// Final pending-speech dispatch at session stop. Any source whose stop-time
/// `VadAction::Chunk` was skipped because a previous task was still running
/// now has un-transcribed audio between its frozen `speech_start_pos` and the
/// current write position. Run one synchronous chunk per such source so the
/// final segment lands before finalization.
async fn dispatch_final_pending_chunks(
    sources: &mut [SourceVadState],
    audio_state: &AudioManagerState,
    ctx: &TranscriptionContext,
    counters: &Arc<StdMutex<SessionCounters>>,
    prompt: &Arc<StdMutex<PromptState>>,
    max_chunk_duration: Duration,
) {
    // Same overlap-based guard as the stop-positions final-flush path:
    // skip mic when the pending mic range overlaps any dictation window.
    // `dispatch_final_pending_chunks` runs without an explicit stop limit,
    // so we use the current write position as the upper bound.
    let skip_mic = matches!(ctx.config.source_kind, LiveSourceKind::Session) && {
        let snapshot = ctx.dictation_owns_mic.snapshot();
        let mic_source = sources
            .iter()
            .find(|s| matches!(s.label, AudioSourceLabel::Mic));
        match mic_source {
            Some(s) => {
                let write_pos = {
                    let manager = audio_state.lock().await;
                    source_write_pos(&manager, &s.label)
                };
                snapshot.overlaps(s.speech_start_pos as u64, write_pos as u64)
            }
            None => false,
        }
    };
    for source in sources.iter_mut() {
        if skip_mic && matches!(source.label, AudioSourceLabel::Mic) {
            // Same final-flush mic guard as the stop-positions path —
            // dictation owns the mic, no legitimate session-mic content
            // to flush at finalization.
            continue;
        }
        loop {
            if source.has_in_flight_task {
                break;
            }
            let has_pending = {
                let manager = audio_state.lock().await;
                let write_pos = source_write_pos(&manager, &source.label);
                write_pos > source.speech_start_pos
            };
            if !has_pending {
                break;
            }
            debug!(
                "live transcription stop: dispatching final pending chunk for {:?} \
                 (speech_start_pos={}, bounded drain)",
                source.label, source.speech_start_pos
            );
            let prepared = prepare_chunk_dispatch(
                source,
                audio_state,
                true,
                JobOrigin::FinalFlush,
                max_chunk_duration,
            )
            .await;
            let Some(prepared) = prepared else { break };
            let source_label = prepared.source_label;
            let final_task =
                run_chunk_task(prepared, ctx.clone(), counters.clone(), prompt.clone());
            match tokio::time::timeout(Duration::from_secs(10), final_task).await {
                Ok(outcome) => {
                    source.accumulated_text = outcome.accumulated_text;
                    source.has_in_flight_task = false;
                    if outcome.sidecar_dead {
                        warn!("final pending chunk reported sidecar dead");
                        break;
                    }
                }
                Err(_) => {
                    warn!(
                        "live transcription stop: final pending chunk for {:?} exceeded 10s — \
                         segment may be lost",
                        source_label
                    );
                    break;
                }
            }
            if !source.force_drain {
                break;
            }
        }
    }
}

/// Outcome of a single session-WAV write attempt within the live loop. The
/// only failure the loop cares about is a resample error: when that happens,
/// the loop skips the rest of the tick so it doesn't act on a half-flushed
/// state. Every other path (no `SessionWavState`, write success, write error)
/// is "carry on" from the loop's perspective.
enum WavWriteOutcome {
    Wrote,
    ResampleFailed,
}

/// Append the just-extracted ring-buffer samples to the streaming session WAV.
/// Resamples on the fly when the live extraction sample rate diverges from the
/// header rate (mid-session device rebind), and bumps the periodic-diagnostic
/// counter on success. `flush_positions` is always advanced — retrying a
/// partial write would duplicate samples on the next tick.
fn write_session_wav_samples(
    ws: &mut SessionWavState,
    samples: &[f32],
    sr: u32,
    new_pos: BufferPositions,
) -> WavWriteOutcome {
    let to_write: std::borrow::Cow<[f32]> = if sr == ws.wav_sample_rate {
        std::borrow::Cow::Borrowed(samples)
    } else {
        match yapstack_common::audio::resample(samples, sr, ws.wav_sample_rate) {
            Ok(cow) => std::borrow::Cow::Owned(cow.into_owned()),
            Err(e) => {
                error!(
                    "WAV resample {}Hz → {}Hz failed, dropping {} samples: {}",
                    sr,
                    ws.wav_sample_rate,
                    samples.len(),
                    e
                );
                // Advance positions regardless — retrying would re-extract the
                // same samples next tick.
                ws.flush_positions = new_pos;
                return WavWriteOutcome::ResampleFailed;
            }
        }
    };
    if let Err(e) = ws.writer.write_samples(&to_write) {
        error!(
            "session WAV write error ({} samples may be lost): {}",
            to_write.len(),
            e
        );
    }
    ws.flush_positions = new_pos;
    ws.flush_count += 1;
    if ws.flush_count.is_multiple_of(WAV_FLUSH_DIAGNOSTIC_INTERVAL) {
        debug!(
            "session WAV progress: flushes={}, samples_written={}, duration={:.1}s",
            ws.flush_count,
            ws.writer.samples_written(),
            ws.writer.duration_seconds()
        );
    }
    WavWriteOutcome::Wrote
}

/// Advance the consecutive-empty-flush counter and emit the layered
/// diagnostics: a one-shot user-facing event the first time the count crosses
/// `WAV_FLUSH_ERROR_THRESHOLD` (deferred until backfill completes so the
/// backfill-driven empty window doesn't false-positive), and a periodic warn
/// log every `WAV_FLUSH_WARNING_INTERVAL` ticks thereafter.
async fn handle_empty_wav_flush(
    wav_flush_none_count: &mut u32,
    session_wav_state: Option<&SessionWavState>,
    audio_state: &AudioManagerState,
    ctx: &TranscriptionContext,
    backfill_done: &Arc<AtomicBool>,
) {
    *wav_flush_none_count += 1;
    debug!(
        "session WAV flush: no data (consecutive: {})",
        *wav_flush_none_count
    );
    if *wav_flush_none_count == WAV_FLUSH_ERROR_THRESHOLD {
        if backfill_done.load(Ordering::Acquire) {
            if let Some(ws) = session_wav_state {
                let has_stream_error = {
                    let mgr = audio_state.lock().await;
                    mgr.system_has_stream_error()
                };
                match classify_empty_flush(ws.source, has_stream_error) {
                    EmptyFlushClassification::WindowsSilence => {
                        info!(
                            "session WAV: empty extractions for SystemOnly session {} on Windows — emitting warning (no stream error)",
                            ws.session_id
                        );
                        let _ = ctx.app_handle.emit(
                            "session-wav-warning",
                            SessionWavWarningEvent {
                                session_id: ws.session_id.clone(),
                                message:
                                    "No system audio detected — recording will resume when audio plays"
                                        .to_string(),
                            },
                        );
                    }
                    EmptyFlushClassification::Error => {
                        warn!(
                            "session WAV: 10 consecutive empty extractions for session {} — emitting error event",
                            ws.session_id
                        );
                        let _ = ctx.app_handle.emit(
                            "session-wav-error",
                            SessionWavErrorEvent {
                                session_id: ws.session_id.clone(),
                                message:
                                    "No audio data available for recording — audio may not be saved"
                                        .to_string(),
                            },
                        );
                    }
                }
            }
        } else {
            debug!("session WAV flush: resetting empty count (backfill still running)");
            *wav_flush_none_count = 0;
        }
    }
    if wav_flush_none_count.is_multiple_of(WAV_FLUSH_WARNING_INTERVAL) {
        let silence_secs = *wav_flush_none_count as f32 * POLL_INTERVAL_MS as f32 / 1000.0;
        warn!(
            "session WAV flush: {} consecutive empty extractions ({:.1}s) — possible sample rate mismatch or no audio data",
            *wav_flush_none_count, silence_secs
        );
    }
}

/// Seed the live shared prompt and per-source `accumulated_text` from the
/// backfill task's bridged prompt the first time it's non-empty. Whisper uses
/// `accumulated_text` as its initial prompt, so seeding both keeps the live
/// loop's first chunk aware of what backfill already transcribed. Returns
/// `true` once the seed lands so the caller can flip the one-shot guard.
async fn seed_prompt_from_backfill(
    ctx: &TranscriptionContext,
    prompt: &Arc<StdMutex<PromptState>>,
    sources: &mut [SourceVadState],
) -> bool {
    let bridged = ctx.bridged_prompt.lock().await;
    if bridged.is_empty() {
        return false;
    }
    {
        let mut p = prompt.lock().expect("prompt mutex poisoned");
        if p.shared_prompt.is_empty() {
            p.shared_prompt = bridged.clone();
        }
        p.last_transcription_at = Some(Instant::now());
    }
    for source in sources.iter_mut() {
        if source.accumulated_text.is_empty() {
            source.accumulated_text = bridged.clone();
        }
    }
    debug!(
        "live loop: seeded prompt from backfill ({} chars)",
        bridged.len()
    );
    true
}

/// Flush any remaining session audio. When `limit` is present, reads are
/// bounded to the stop snapshot so post-stop samples cannot enter the part.
/// Mutes mic samples in the extracted batch when the session's mic source
/// observed dictation owning the mic — same per-tick approximation
/// `mute_dictation_window_in_wav_extraction` uses, propagated to the
/// stop-time flush so an in-flight dictation can't leak audio into the
/// session WAV during the final drain.
async fn flush_session_wav_to_limit(
    ws: &mut SessionWavState,
    audio_state: &AudioManagerState,
    limit: Option<&BufferPositions>,
    ctx: &TranscriptionContext,
) {
    let (final_flush, mic_channels, old_mic_pos) = {
        let manager = audio_state.lock().await;
        let old_mic_pos = ws.flush_positions.mic_pos as u64;
        let mic_channels = manager.mic_buffer().map(|b| b.channels()).unwrap_or(1) as usize;
        let extraction = match limit {
            Some(limit) => manager.extract_since_until(
                &ws.flush_positions,
                limit,
                ws.source,
                ws.mix_config.as_ref(),
            ),
            None => manager
                .extract_since(&ws.flush_positions, ws.source, ws.mix_config.as_ref())
                .map(
                    |(samples, sample_rate, new_positions)| yapstack_audio::BoundedExtraction {
                        samples,
                        sample_rate,
                        new_positions,
                        overrun: false,
                    },
                ),
        };
        (extraction, mic_channels, old_mic_pos)
    };
    if let Some(mut flush) = final_flush {
        // Mute samples in any dictation-window overlap. Same helper the
        // per-tick flush uses, so the precise window-overlap math stays
        // consistent across both flush paths.
        mute_dictation_window_in_wav_extraction(
            &mut flush.samples,
            mic_channels,
            old_mic_pos,
            ctx,
            ws.source,
        );
        if flush.overrun {
            warn!(
                marker = "session_wav_stop_overrun",
                session_id = %ws.session_id,
                "ring buffer overrun while flushing session WAV to stop boundary"
            );
        }
        let to_write: std::borrow::Cow<[f32]> = if flush.sample_rate == ws.wav_sample_rate {
            std::borrow::Cow::Borrowed(&flush.samples)
        } else {
            match yapstack_common::audio::resample(
                &flush.samples,
                flush.sample_rate,
                ws.wav_sample_rate,
            ) {
                Ok(cow) => std::borrow::Cow::Owned(cow.into_owned()),
                Err(e) => {
                    error!(
                        "session WAV final flush resample {}Hz → {}Hz failed, dropping {} samples: {}",
                        flush.sample_rate,
                        ws.wav_sample_rate,
                        flush.samples.len(),
                        e
                    );
                    std::borrow::Cow::Owned(Vec::new())
                }
            }
        };
        if !to_write.is_empty() {
            if let Err(e) = ws.writer.write_samples(&to_write) {
                error!("session WAV final flush write failed: {}", e);
            }
        }
        ws.flush_positions = flush.new_positions;
    }
}

/// Finalize the session-WAV writer in the user's chosen export format and
/// persist the resulting part row to the DB before emitting
/// `session-part-ready`. The caller must perform the final bounded/unbounded
/// flush before invoking this.
async fn finalize_session_wav(ws: SessionWavState, ctx: &TranscriptionContext) {
    if ws.writer.samples_written() == 0 {
        warn!(
            "session WAV had 0 samples written — deleting empty file for session {}",
            ws.session_id
        );
        let wav_path = ws.writer.path().to_path_buf();
        // Finalize WAV only (no MP3 conversion) to release the file handle, then delete.
        let _ = ws.writer.finalize_wav_only();
        let _ = std::fs::remove_file(&wav_path);
        let _ = ctx.app_handle.emit(
            "session-wav-error",
            SessionWavErrorEvent {
                session_id: ws.session_id,
                message: "No audio was recorded — WAV file not saved".to_string(),
            },
        );
        return;
    }

    let format = ctx
        .config
        .audio_export_format
        .unwrap_or(AudioExportFormatDto::Mp3);
    let result = if format.is_mp3() {
        ws.writer
            .finalize_as_mp3(ctx.config.mp3_bitrate.unwrap_or(64))
    } else {
        ws.writer.finalize_wav_only()
    };
    let (path, duration) = match result {
        Ok(out) => out,
        Err(e) => {
            error!("session WAV finalize failed: {}", e);
            return;
        }
    };

    info!(
        "session part {} finalized: {} ({:.1}s)",
        ws.part_index,
        path.display(),
        duration
    );
    let file_path_str = path.to_string_lossy().to_string();

    // Always register the parent dir with TrustedAudioDirs so the
    // audio-stream:// handler can serve the file regardless of who owns
    // the audio (sessions vs dictations).
    if let Some(parent) = path.parent() {
        crate::register_trusted_audio_dir(&ctx.app_handle, parent);
    }

    // Insert the parts row from Rust *before* emitting so the DB stays the
    // durable source of truth even if the FE event listener is unavailable
    // (crash, force-quit, window closed). The FE handler then just
    // refreshes from DB. Skipped when `persist_audio_part` is false —
    // dictation owns its own audio path on `dictation_history` and the
    // synthetic `session_id` has no row in `sessions`, so inserting would
    // either FK-fail or (with FK enforcement off) leave orphans that
    // `clearAllSessions` could then sweep.
    if ws.persist_audio_part {
        if let Some(db_path_state) = ctx.app_handle.try_state::<crate::DbPath>() {
            let row = crate::db::AudioPartRow {
                session_id: ws.session_id.clone(),
                part_index: ws.part_index,
                file_path: file_path_str.clone(),
                format: format.as_str(),
                duration_seconds: duration,
                sample_rate: ws.wav_sample_rate,
            };
            if let Err(e) = crate::db::insert_audio_part_row(db_path_state.as_path(), &row) {
                error!("insert_audio_part_row failed (will rely on FE refresh): {e}");
            }
        }
    }

    let _ = ctx.app_handle.emit(
        "session-part-ready",
        SessionPartReadyEvent {
            session_id: ws.session_id,
            part_index: ws.part_index,
            file_path: file_path_str,
            format,
            duration_seconds: duration,
            sample_rate: ws.wav_sample_rate,
        },
    );
}

/// Build the `initial_prompt` we'll send to the engine for one chunk:
/// `"<vocabulary_hints>. <rolling_context>"`. Vocabulary hints (folder/tag
/// names) prime Whisper for proper nouns and are read fresh per chunk so
/// mid-recording updates take effect; the hints are snapshotted into an owned
/// string inside a tight scope so the mutex is dropped before the long
/// `transcribe_with` await blocks any concurrent `update_vocabulary_hints`
/// call. Returns `None` only when both vocab and accumulated context are
/// empty.
async fn build_effective_prompt(
    ctx: &TranscriptionContext,
    accumulated_text: &str,
) -> Option<String> {
    let max_prompt = ctx.config.prompt_context_chars.unwrap_or(350) as usize;
    let vocab_truncated: String = {
        let vocab_guard = ctx.vocabulary_hints.lock().await;
        let vocab: &str = vocab_guard.as_str();
        // Round down to a char boundary — folder/tag names can contain
        // multibyte codepoints, and a raw byte slice at the 80-byte cap could
        // land mid-codepoint and panic the live-transcription task.
        let vocab_budget = vocab.floor_char_boundary(vocab.len().min(80));
        vocab[..vocab_budget].to_string()
    };
    let vocab_budget = vocab_truncated.len();
    let context_budget = max_prompt.saturating_sub(if vocab_budget > 0 {
        vocab_budget + 2
    } else {
        0
    });

    if accumulated_text.is_empty() && vocab_budget == 0 {
        return None;
    }

    let context_part = if accumulated_text.is_empty() {
        ""
    } else if accumulated_text.len() > context_budget {
        let boundary = accumulated_text.ceil_char_boundary(accumulated_text.len() - context_budget);
        &accumulated_text[boundary..]
    } else {
        accumulated_text
    };

    if vocab_budget > 0 && !context_part.is_empty() {
        Some(format!("{}. {}", vocab_truncated, context_part))
    } else if vocab_budget > 0 {
        Some(vocab_truncated)
    } else {
        Some(context_part.to_string())
    }
}

/// Transcribes a chunk of audio and returns a `TranscribeOutcome`.
///
/// - `Success`: transcription produced segments
/// - `Skipped`: non-fatal failure (temp file error, empty chunk, transient engine error,
///   or a `Cancelled` outcome from the scheduler during shutdown)
/// - `SidecarDead`: scheduler reports the sidecar died and could not be restarted
async fn transcribe_chunk(
    ctx: &TranscriptionContext,
    input: &ChunkInput<'_>,
    chunk_index: &mut u32,
    accumulated_text: &mut String,
    prompt: &Arc<StdMutex<PromptState>>,
) -> TranscribeOutcome {
    let chunk_duration = input.samples.len() as f32 / input.sample_rate as f32;
    if chunk_duration < MIN_CHUNK_DURATION_SECS {
        return TranscribeOutcome::Skipped;
    }

    // Write temp WAV — guard ensures cleanup on all exit paths including panics.
    let _wav_guard;
    let wav_path =
        match yapstack_audio::export::write_wav_to_temp(input.samples, input.sample_rate, 1) {
            Ok(p) => {
                _wav_guard = TempFileGuard(p.clone());
                p
            }
            Err(e) => {
                error!("live transcription: failed to write WAV: {}", e);
                return TranscribeOutcome::Skipped;
            }
        };

    let effective_prompt = build_effective_prompt(ctx, accumulated_text).await;

    // initial_prompt is Whisper-only — Parakeet's TDT decoder has no text-prompt
    // input, so passing it would just be ignored. Drop it explicitly so the IPC
    // payload is honest about what was sent. Engine identity comes from the
    // session-resolved profile so we don't have to re-read `client.engine()`
    // and so future engines can opt in via a single boolean.
    let profile = ctx.engine_profile.as_ref();
    let engine_kind = profile.engine_kind;
    let prompt_for_engine = if profile.uses_initial_prompt {
        effective_prompt
    } else {
        None
    };

    // Submit to the scheduler. The scheduler owns the sole TranscriptionClient
    // Arc and serializes calls to `transcribe_with`, with priority ordering
    // (FinalFlush > Dictation > Live > Backfill) and mic/system round-robin
    // at the live tier. Respawn on sidecar death is handled inside the
    // scheduler — when it returns `SchedulerError::SidecarDead`, the respawn
    // already failed.
    let rx = match ctx.scheduler.submit(JobRequest {
        origin: input.origin,
        source: source_from_label(input.source_label),
        wav_path: wav_path.clone(),
        language: ctx.config.language.clone(),
        initial_prompt: prompt_for_engine,
        diarization: ctx.config.diarization,
    }) {
        Ok(rx) => rx,
        Err(e) => {
            warn!(
                "live transcription: scheduler refused submit ({}) — treating as sidecar dead",
                e
            );
            return TranscribeOutcome::SidecarDead;
        }
    };

    let outcome = match rx.await {
        Ok(o) => o,
        Err(_) => {
            warn!("live transcription: scheduler dropped response — shutting down");
            return TranscribeOutcome::SidecarDead;
        }
    };

    let wall_ms = outcome.wall_ms;
    let transcription_result = outcome.result;

    // Emit pressure telemetry regardless of outcome — a chunk that took 8s
    // to fail is just as much "the pipeline is unhealthy" as one that took 8s
    // to succeed. RTFx is only meaningful on success and when wall_ms > 0.
    let rtfx = match (&transcription_result, wall_ms) {
        (Ok(_), w) if w > 0 => Some(chunk_duration / (w as f32 / 1000.0)),
        _ => None,
    };
    let lag_seconds = if transcription_result.is_ok() {
        let session_time_now =
            ctx.session_offset_base_seconds + ctx.session_start_instant.elapsed().as_secs_f32();
        let chunk_end_session_time = input.audio_offset_seconds + chunk_duration;
        Some((session_time_now - chunk_end_session_time).max(0.0))
    } else {
        None
    };
    let _ = ctx.app_handle.emit(
        "live-transcription-pressure",
        LiveTranscriptionPressureEvent {
            source: input.source_label,
            chunk_index: *chunk_index,
            chunk_audio_seconds: chunk_duration,
            wall_ms,
            rtfx,
            engine: profile.engine_name.to_string(),
            origin: input.origin.into(),
            lag_seconds,
            drain_backlog_seconds: input.drain_backlog_seconds,
            accel: profile.accel.clone(),
            variant: profile.variant.clone(),
        },
    );
    // Structured `info!` mirror of the pressure event so the data lands in
    // the in-app log buffer even when no JS listener is mounted. Every
    // field is grep-friendly and survives across days of usage if log
    // retention does. The "live_pressure" marker makes it easy to filter:
    //   grep 'live_pressure' app.log | jq -s ...
    info!(
        marker = "live_pressure",
        source = ?input.source_label,
        chunk_index = *chunk_index,
        chunk_audio_secs = chunk_duration,
        wall_ms = wall_ms,
        rtfx = ?rtfx,
        lag_seconds = ?lag_seconds,
        drain_backlog_seconds = input.drain_backlog_seconds,
        engine = profile.engine_name,
        accel = profile.accel.as_deref(),
        variant = profile.variant.as_deref(),
        origin = input.origin.as_str(),
        ok = transcription_result.is_ok(),
        "transcribe chunk pressure"
    );

    // _wav_guard handles cleanup via Drop

    match transcription_result {
        Ok(result) => {
            *chunk_index += 1;

            // Build prompt-safe text by excluding hallucination patterns.
            // Even if the sidecar accepted segments (e.g. marginal fillers at high
            // confidence), we don't want them priming the next chunk's initial_prompt
            // and creating a feedback loop.
            let prompt_text: String = result
                .segments
                .iter()
                .map(|s| s.text.as_str())
                .filter(|t| !yapstack_common::hallucination::is_always_reject(t, engine_kind))
                .collect::<Vec<_>>()
                .join(" ");

            if !prompt_text.is_empty() {
                if !accumulated_text.is_empty() {
                    accumulated_text.push(' ');
                }
                accumulated_text.push_str(&prompt_text);
                // Cap accumulated_text to prevent unbounded growth over long sessions
                if accumulated_text.len() > MAX_ACCUMULATED_TEXT_CHARS {
                    let boundary = accumulated_text
                        .ceil_char_boundary(accumulated_text.len() - MAX_ACCUMULATED_TEXT_CHARS);
                    *accumulated_text = accumulated_text[boundary..].to_string();
                }
                let max_prompt = ctx.config.prompt_context_chars.unwrap_or(350) as usize;
                let mut p = prompt.lock().expect("prompt mutex poisoned");
                if !p.shared_prompt.is_empty() {
                    p.shared_prompt.push(' ');
                }
                p.shared_prompt.push_str(&prompt_text);
                if p.shared_prompt.len() > max_prompt {
                    let boundary = p
                        .shared_prompt
                        .ceil_char_boundary(p.shared_prompt.len() - max_prompt);
                    p.shared_prompt = p.shared_prompt[boundary..].to_string();
                }
            }

            let segments: Vec<TranscriptSegmentDto> = result
                .segments
                .into_iter()
                .map(|s| TranscriptSegmentDto {
                    start_ms: s.start_ms,
                    end_ms: s.end_ms,
                    text: s.text,
                    confidence: s.confidence,
                    speaker_id: s.speaker_id,
                })
                .collect();

            info!(
                "transcribed: {:?} chunk {} origin={} offset={:.2}s {:.1}s audio {} chars",
                input.source_label,
                *chunk_index,
                input.origin.as_str(),
                input.audio_offset_seconds,
                chunk_duration,
                result.text.len()
            );

            TranscribeOutcome::Success(ChunkResult {
                event: LiveSegmentEvent {
                    chunk_index: *chunk_index - 1,
                    source: input.source_label,
                    segments,
                    audio_offset_seconds: input.audio_offset_seconds,
                    chunk_duration_seconds: chunk_duration,
                    accumulated_text: accumulated_text.clone(),
                    origin: input.origin.into(),
                    source_kind: ctx.config.source_kind,
                    session_id: ctx.config.session_id.clone(),
                },
                chunk_duration,
                wall_ms,
            })
        }
        Err(SchedulerError::SidecarDead) => {
            error!("live transcription: scheduler reports sidecar dead");
            let _ = ctx.app_handle.emit(
                "live-transcription-status",
                LiveTranscriptionStatus {
                    phase: LiveTranscriptionPhase::Error,
                    chunks_processed: *chunk_index,
                    total_audio_seconds: 0.0,
                    error_message: Some(
                        "Transcription engine stopped unexpectedly and could not be restarted"
                            .to_string(),
                    ),
                    session_id: ctx.config.session_id.clone(),
                    effective_start_epoch_ms: None,
                    lag_seconds: None,
                    live_drain_backlog_chunks: 0,
                    live_drain_backlog_seconds: 0.0,
                    source_kind: ctx.config.source_kind,
                },
            );
            TranscribeOutcome::SidecarDead
        }
        Err(SchedulerError::Cancelled) | Err(SchedulerError::Shutdown) => {
            // `Cancelled` fires when the scheduler was already shutting down
            // when this chunk reached the front of the queue. `Shutdown`
            // fires when `submit` is called after the scheduler has begun
            // tearing down (e.g. a clone held by a racing stop path). Both
            // mean the engine is going away; no warning to surface.
            debug!(
                "live transcription: scheduler cancelled/shutdown chunk for {:?} during teardown",
                input.source_label
            );
            TranscribeOutcome::Skipped
        }
        Err(SchedulerError::Transcription(msg)) => {
            warn!("live transcription: chunk failed: {msg}, skipping");
            // The scheduler already attempted a single respawn-and-retry on
            // sidecar death; reaching here means a transient engine error or
            // a post-respawn retry that still failed. Surface a user-visible
            // warning and let the caller quarantine the audio.
            let _ = ctx.app_handle.emit(
                "live-transcription-warning",
                LiveTranscriptionWarningEvent {
                    message: "Transcription engine error — chunk skipped".into(),
                },
            );
            TranscribeOutcome::Skipped
        }
    }
}

/// Transcribe a chunk, emit segment/status events, and return the outcome.
///
/// The caller uses the outcome to manage VAD cursor state:
/// - `Success`: advance cursor past transcribed audio
/// - `Skipped`: keep speech_start_pos for retry (audio not lost)
/// - `SidecarDead`: fatal — caller should break the loop
async fn transcribe_and_emit_chunk(
    ctx: &TranscriptionContext,
    input: &ChunkInput<'_>,
    chunk_index: &mut u32,
    accumulated_text: &mut String,
    counters: &Arc<StdMutex<SessionCounters>>,
    prompt: &Arc<StdMutex<PromptState>>,
) -> TranscribeOutcome {
    let outcome = transcribe_chunk(ctx, input, chunk_index, accumulated_text, prompt).await;

    if let TranscribeOutcome::Success(ref result) = outcome {
        // Short critical section — no await held.
        let (total_chunks, total_audio_seconds, backlog_chunks, backlog_seconds) = {
            let mut s = counters.lock().expect("counters mutex poisoned");
            s.total_chunks += 1;
            s.total_audio_seconds += result.chunk_duration;
            s.total_wall_ms = s.total_wall_ms.saturating_add(result.wall_ms);
            // Track the highest session-time end we've ever transcribed so
            // the status command can compute live lag against the wall clock.
            // Take the max rather than last-write: backfill chunks land in
            // arbitrary order against live, so a backfill chunk for older
            // audio can complete after a live chunk for newer audio. Last-
            // write would clobber the counter backwards and overstate lag
            // by the live-vs-backfill offset gap. Pressure event already
            // reports per-chunk lag; this is for the polled-status surface
            // used by StatusPopover.
            let chunk_end = result.event.audio_offset_seconds + result.chunk_duration;
            s.latest_completed_audio_offset_seconds = Some(
                s.latest_completed_audio_offset_seconds
                    .map_or(chunk_end, |prev| prev.max(chunk_end)),
            );
            (
                s.total_chunks,
                s.total_audio_seconds,
                s.live_drain_backlog_chunks,
                s.live_drain_backlog_seconds,
            )
        };
        prompt
            .lock()
            .expect("prompt mutex poisoned")
            .last_transcription_at = Some(Instant::now());

        // Compute lag for the status broadcast below — same formula as the
        // pressure event but read from ctx so emit_status can stay engine-
        // and chunk-agnostic.
        let session_time_now =
            ctx.session_offset_base_seconds + ctx.session_start_instant.elapsed().as_secs_f32();
        let chunk_end_session_time = result.event.audio_offset_seconds + result.chunk_duration;
        let lag_seconds = Some((session_time_now - chunk_end_session_time).max(0.0));

        let _ = ctx
            .app_handle
            .emit("live-transcription-segment", &result.event);
        emit_status(
            ctx,
            LiveTranscriptionPhase::Running,
            StatusProgress {
                chunks: total_chunks,
                audio_secs: total_audio_seconds,
                lag_seconds,
                live_drain_backlog_chunks: backlog_chunks,
                live_drain_backlog_seconds: backlog_seconds,
            },
        );
    }

    outcome
}

// --- Tauri commands ---

#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
pub async fn start_live_transcription(
    audio_state: tauri::State<'_, AudioManagerState>,
    scheduler_state: tauri::State<'_, TranscriptionSchedulerState>,
    live_state: tauri::State<'_, LiveTranscriptionState>,
    dictation_owns_mic: tauri::State<'_, super::transcription::DictationOwnsMicState>,
    restart_intent_inbox: tauri::State<'_, RestartIntentInbox>,
    live_session_present: tauri::State<'_, LiveSessionPresent>,
    app_handle: AppHandle,
    mut config: LiveTranscriptionConfig,
) -> Result<LiveTranscriptionStartResult, CommandError> {
    // Cheap config validation first — no locks held. `is_finite` alongside
    // the sign checks because NaN comparisons all return false; raw
    // `<= 0.0` silently admits NaN payloads which would later panic in
    // sample-count math.
    if matches!(config.source_kind, LiveSourceKind::Dictation) {
        if !matches!(config.source, CaptureSourceDto::MicOnly) {
            return Err(CommandError::InvalidInput {
                message: "dictation requires source=MicOnly".into(),
            });
        }
        if config.backfill_seconds != 0.0 {
            return Err(CommandError::InvalidInput {
                message: "dictation requires backfill_seconds=0".into(),
            });
        }
        if config.persist_audio_part {
            return Err(CommandError::InvalidInput {
                message: "dictation requires persist_audio_part=false".into(),
            });
        }
    }
    if config.silence_duration_ms == 0 {
        return Err(CommandError::InvalidInput {
            message: "silence_duration_ms must be > 0".into(),
        });
    }
    if !config.max_chunk_seconds.is_finite() || config.max_chunk_seconds <= 0.0 {
        return Err(CommandError::InvalidInput {
            message: "max_chunk_seconds must be finite and > 0".into(),
        });
    }
    if !config.backfill_seconds.is_finite() || config.backfill_seconds < 0.0 {
        return Err(CommandError::InvalidInput {
            message: "backfill_seconds must be finite and >= 0".into(),
        });
    }
    if let Some(decay) = config.prompt_decay_silence_seconds {
        if !decay.is_finite() || decay < 0.0 {
            return Err(CommandError::InvalidInput {
                message: "prompt_decay_silence_seconds must be finite and >= 0".into(),
            });
        }
    }
    // Validate MP3 bitrate now so a misconfigured session doesn't fail late,
    // after the buffer has been drained into the session WAV with no recovery.
    let will_write_mp3 = config
        .audio_export_format
        .unwrap_or(AudioExportFormatDto::Mp3)
        .is_mp3();
    if will_write_mp3 {
        if let Some(kbps) = config.mp3_bitrate {
            yapstack_audio::export::validate_mp3_bitrate(kbps).map_err(|e| {
                CommandError::InvalidInput {
                    message: e.to_string(),
                }
            })?;
        }
    }

    // Pre-read mic_pos for dictation under audio_state alone, BEFORE
    // taking live_state. Lock order is `AudioManager → ... →
    // LiveTranscription`, so reading audio inside the live_state guard
    // (as the prior implementation did) violated the documented order.
    // The brief staleness between this read and the live_state
    // reservation a few lines below is irrelevant — at most a few mic
    // frames written in microseconds.
    let dictation_start_pos: Option<u64> =
        if matches!(config.source_kind, LiveSourceKind::Dictation) {
            let manager = audio_state.lock().await;
            Some(
                manager
                    .mic_buffer_info()
                    .map(|i| i.samples_written as u64)
                    .unwrap_or(0),
            )
        } else {
            None
        };

    // Brief live_state lock: reject same-kind double-start, defensive-
    // clear stale dictation flag (gated), open the dictation mic-
    // ownership window, reserve the slot as `Starting`. Capture
    // `skip_preflight` while we have the lock so the rest of this
    // function can run without needing live_state for that decision.
    //
    // The slot stays non-`Idle` for the full lifetime including
    // finalization; the slot guard rejects any racing same-kind start
    // including starts that arrive while we're in `Starting`. The
    // returned `StartingSlotRevertGuard`'s Drop spawns a detached
    // task to flip Starting → Idle if any setup step below errors out.
    // On success we install Running atomically at the end and disarm
    // the revert guard.
    let (mic_ownership_guard, mut starting_revert_guard, skip_preflight) = {
        let mut guard = live_state.lock().await;
        if !guard.slot(config.source_kind).is_idle() {
            // Drop dictation_start_pos — no window was opened yet.
            return Err(CommandError::InvalidInput {
                message: format!(
                    "live transcription ({}) is already running or finalizing",
                    config.source_kind.as_str()
                ),
            });
        }
        if matches!(config.source_kind, LiveSourceKind::Session)
            && guard.slot(LiveSourceKind::Dictation).is_idle()
        {
            // Defensive clear for the rare case where a prior dictation
            // panic left state behind. Only safe when the dictation slot
            // is Idle — otherwise we'd unwind a live dictation's open
            // window and let the session ingest dictation audio.
            dictation_owns_mic.clear_for_session();
        }
        let mic_guard: Option<MicOwnershipGuard> = if let Some(start) = dictation_start_pos {
            dictation_owns_mic.open(start);
            Some(MicOwnershipGuard {
                state: dictation_owns_mic.inner().clone(),
            })
        } else {
            None
        };
        // `skip_preflight` is captured here while we still have the
        // live_state lock — preflight can replace the mic buffer
        // (`RestartOutcome::BufferReplaced`), and only the session's
        // in-loop restart handler resets `SourceVadState` cursors +
        // session-WAV `flush_positions` on replacement. A preflight-
        // driven replace from a parallel dictation start would leave
        // an active session reading stale positions, so dictation
        // defers to the session's stream-health watchdog when a
        // session is live.
        let skip_preflight = matches!(config.source_kind, LiveSourceKind::Dictation)
            && !guard.slot(LiveSourceKind::Session).is_idle();
        *guard.slot_mut(config.source_kind) = RuntimeSlot::Starting;
        let revert_guard = StartingSlotRevertGuard {
            state: live_state.inner().clone(),
            kind: config.source_kind,
            armed: true,
        };
        (mic_guard, revert_guard, skip_preflight)
    };

    // Pre-flight: restart any silently-stalled capture streams before we
    // read backfill or spawn the loop. Without this, the first dictation
    // after a long idle (device change, Bluetooth drop, OS sleep)
    // transcribes whatever stale audio happens to be in the ring buffer.
    let preflight_source: CaptureSource = config.source.clone().into();
    if !skip_preflight {
        preflight_stream_health(audio_state.inner(), &preflight_source, &app_handle).await?;
    }

    // Resuming forces backfill to 0 — new audio is captured fresh, not
    // dredged from the ring buffer.
    if config.resume.is_some() {
        config.backfill_seconds = 0.0;
    }
    let session_offset_base_seconds = config
        .resume
        .as_ref()
        .map(|r| r.offset_base_seconds.max(0.0))
        .unwrap_or(0.0);

    // Clamp backfill to actual audio available in the buffer.
    let effective_backfill_seconds = {
        let manager = audio_state.lock().await;
        let source = config.source.clone().into();

        // Log sample rate diagnostic for mixed mode
        if matches!(source, CaptureSource::Mixed) {
            if let (Some(mic), Some(sys)) =
                (manager.mic_buffer_info(), manager.system_buffer_info())
            {
                if mic.sample_rate != sys.sample_rate {
                    info!(
                        "Mixed mode: mic={}Hz/{}ch, system={}Hz/{}ch — will resample during extraction",
                        mic.sample_rate, mic.channels, sys.sample_rate, sys.channels
                    );
                }
            }
        }

        if config.backfill_seconds > 0.0 {
            let mic_avail = if matches!(source, CaptureSource::MicOnly | CaptureSource::Mixed) {
                manager.mic_buffer_info().map(|i| i.available_seconds)
            } else {
                None
            };
            let sys_avail = if matches!(source, CaptureSource::SystemOnly | CaptureSource::Mixed) {
                manager.system_buffer_info().map(|i| i.available_seconds)
            } else {
                None
            };
            let available = match (mic_avail, sys_avail) {
                (Some(m), Some(s)) => m.min(s),
                (Some(v), None) | (None, Some(v)) => v,
                (None, None) => 0.0,
            };
            yapstack_audio::ring_buffer::clamp_backfill_to_available(
                config.backfill_seconds,
                available,
            )
        } else {
            0.0
        }
    };

    info!(
        "live transcription starting: requested_backfill={:.1}s effective_backfill={:.1}s",
        config.backfill_seconds, effective_backfill_seconds
    );

    let now_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as f64;
    let effective_start_epoch_ms = now_unix_ms - effective_backfill_seconds as f64 * 1000.0;

    // Set up streaming WAV writer if session_id is provided.
    let session_wav_state =
        if let Some(ref session_id) = config.session_id {
            validate_session_id(session_id)?;
            let audio_dir = if let Some(ref custom_dir) = config.audio_save_location {
                std::path::PathBuf::from(custom_dir)
            } else {
                app_handle
                    .path()
                    .app_data_dir()
                    .map_err(|e: tauri::Error| CommandError::Internal {
                        message: format!("failed to resolve app data dir: {e}"),
                    })?
                    .join("audio")
            };
            std::fs::create_dir_all(&audio_dir)?;

            // Persist the chosen audio dir before writing anything. This way
            // reconciliation on next launch can scan it for orphans even if
            // the run dies between WAV finalize and `insert_audio_part_row`.
            // Also register it as trusted so playback works while the run is
            // still in progress.
            if let Some(db_path_state) = app_handle.try_state::<crate::DbPath>() {
                crate::db::register_audio_save_location(db_path_state.as_path(), &audio_dir);
            }
            crate::register_trusted_audio_dir(&app_handle, &audio_dir);

            // Each recording run produces its own part file. Resume passes the
            // next index; fresh sessions always start at 0.
            let part_index = config.resume.as_ref().map(|r| r.part_index).unwrap_or(0);
            let wav_path = audio_dir.join(format!("{session_id}.{part_index}.wav"));

            let source = config.source.clone().into();
            let sample_rate = {
                let manager = audio_state.lock().await;
                manager.output_sample_rate_for(source)
            };
            let writer = yapstack_audio::SessionWavWriter::new(wav_path.clone(), sample_rate)
                .map_err(|e| CommandError::Internal {
                    message: format!("failed to create session WAV: {e}"),
                })?;

            // Force normalize=false for the streaming WAV writer. Per-chunk normalization
            // (every ~300ms) causes jarring volume discontinuities — quiet passages get
            // amplified enormously while loud passages barely change. The user's normalize
            // setting is meant for the real-time preview, not the archival WAV.
            let mix_config = config
                .mix_config
                .as_ref()
                .map(|mc| yapstack_audio::MixConfig {
                    mic_gain: mc.mic_gain,
                    system_gain: mc.system_gain,
                    normalize: false,
                });

            // Rewind flush positions by backfill so the WAV includes backfill audio
            let flush_positions = {
                let manager = audio_state.lock().await;
                let positions = manager.buffer_positions();
                if effective_backfill_seconds > 0.0 {
                    let mic_rewind = manager
                        .mic_buffer_info()
                        .map(|i| {
                            let raw = (effective_backfill_seconds
                                * i.sample_rate as f32
                                * i.channels as f32) as usize;
                            // Round down to frame boundary for correct deinterleaving
                            raw - (raw % i.channels as usize)
                        })
                        .unwrap_or(0);
                    let sys_rewind = manager
                        .system_buffer_info()
                        .map(|i| {
                            let raw = (effective_backfill_seconds
                                * i.sample_rate as f32
                                * i.channels as f32) as usize;
                            // Round down to frame boundary for correct deinterleaving
                            raw - (raw % i.channels as usize)
                        })
                        .unwrap_or(0);
                    BufferPositions {
                        mic_pos: positions.mic_pos.saturating_sub(mic_rewind),
                        system_pos: positions.system_pos.saturating_sub(sys_rewind),
                    }
                } else {
                    positions
                }
            };

            info!(
                "session WAV writer created: session_id={} sample_rate={}",
                session_id, sample_rate
            );

            Some(SessionWavState {
                writer,
                flush_positions,
                source,
                mix_config,
                session_id: session_id.clone(),
                part_index,
                flush_count: 0,
                wav_sample_rate: sample_rate,
                persist_audio_part: config.persist_audio_part,
            })
        } else {
            None
        };

    let (stop_tx, stop_rx) = oneshot::channel();
    let stop_requested = Arc::new(AtomicBool::new(false));

    // Capture session_id before config is moved into TranscriptionContext
    let controller_session_id = config.session_id.clone();
    let runtime_source_kind = config.source_kind;

    // Channel for RestartIntents from the device broker. The receiver is
    // always consumed by the live loop's select! arm — every runtime needs
    // one for the loop to compile. The *sender* is only published to the
    // global inbox for Session runtimes: dictation is short-lived, mic-only,
    // and never participates in the broker's restart-routing protocol.
    // Without this gate, a dictation start during an active session would
    // overwrite the session loop's installed sender, so subsequent device-
    // change restarts would route to the dictation loop (or hit a closed
    // channel after dictation exits) and the session would never see them.
    let (restart_intent_tx, restart_intent_rx) =
        tokio::sync::mpsc::unbounded_channel::<RestartIntent>();
    if matches!(runtime_source_kind, LiveSourceKind::Session) {
        let mut inbox_guard = restart_intent_inbox
            .inner()
            .lock()
            .expect("restart-intent inbox poisoned");
        *inbox_guard = Some(restart_intent_tx);
    } else {
        // Drop the dictation-owned sender so the rx closes cleanly — no
        // broker can post into it, and the loop's select! arm reads
        // `None` instead of waiting forever on a leaked channel.
        drop(restart_intent_tx);
    }

    let audio_state_clone = audio_state.inner().clone();

    // Align config backfill with the clamped value so WAV writer and transcript
    // cursor share the same time origin (prevents timestamp drift on playback).
    config.backfill_seconds = effective_backfill_seconds;

    // Clone the long-lived scheduler from app-level state. The scheduler
    // outlives the session — both the session live loop and the dictation
    // live loop hold cloned `Arc<Scheduler>` handles, all submitting into
    // the same single-worker queue.
    let scheduler = {
        let guard = scheduler_state.lock().await;
        guard
            .as_ref()
            .ok_or(CommandError::NotInitialized {
                message: "transcription engine not initialized".into(),
            })?
            .scheduler
            .clone()
    };

    let vocab_hints = Arc::new(Mutex::new(
        config.vocabulary_hints.clone().unwrap_or_default(),
    ));

    let session_start_instant = Instant::now();
    // Resolve the engine profile once at session start using the scheduler's
    // cached engine info. After this point, neither the live loop nor
    // `transcribe_chunk` need to re-read engine kind / accel — they consult
    // `ctx.engine_profile` instead.
    let mut profile = profile_for(scheduler.engine(), &config);
    if let Some(info) = scheduler.engine_info().await {
        profile.accel = info.accel;
        profile.variant = info.model_dir.as_deref().and_then(|p| {
            std::path::Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        });
    }
    let engine_profile = Arc::new(profile);
    // Logged at INFO so a default-level log capture pinned to a stalled
    // session always shows which engine + tuning + accel were active.
    info!(
        engine = engine_profile.engine_name,
        silence_ms = engine_profile.vad_tuning.silence_duration.as_millis() as u64,
        poll_ms = engine_profile.vad_tuning.poll_interval.as_millis() as u64,
        pre_roll_ms = engine_profile.vad_tuning.pre_roll.as_millis() as u64,
        max_chunk_secs = engine_profile.vad_tuning.max_chunk_duration.as_secs_f32(),
        uses_initial_prompt = engine_profile.uses_initial_prompt,
        accel = engine_profile.accel.as_deref(),
        variant = engine_profile.variant.as_deref(),
        session_id = ?config.session_id,
        offset_base_secs = session_offset_base_seconds,
        "live transcription engine profile resolved"
    );
    let ctx = TranscriptionContext {
        scheduler: scheduler.clone(),
        app_handle,
        config,
        bridged_prompt: Arc::new(Mutex::new(String::new())),
        vocabulary_hints: vocab_hints.clone(),
        session_offset_base_seconds,
        session_start_instant,
        engine_profile,
        dictation_owns_mic: dictation_owns_mic.inner().clone(),
        dictation_start_pos,
    };

    let counters = Arc::new(StdMutex::new(SessionCounters {
        total_chunks: 0,
        total_audio_seconds: 0.0,
        live_drain_backlog_chunks: 0,
        live_drain_backlog_seconds: 0.0,
        total_wall_ms: 0,
        latest_completed_audio_offset_seconds: None,
    }));

    // Mark the live session as present *before* spawning the loop. The
    // device broker reads this to decide whether a device-change event
    // should route through the inbox (live loop will handle it) or
    // direct-restart through `AudioManager` (no live loop owns audio
    // state). The spawned task clears the flag in its tail so the
    // signal stays `true` across the entire stop/finalize window —
    // including after `stop_live_transcription` clears the inbox.
    // `live_session_present` is a session-only signal (the device broker only
    // routes restarts through it for sessions). Dictation runtimes don't
    // touch it.
    let is_session = matches!(runtime_source_kind, LiveSourceKind::Session);
    if is_session {
        live_session_present.inner().store(true, Ordering::Release);
    }
    let live_session_present_for_task = if is_session {
        Some(Arc::clone(live_session_present.inner()))
    } else {
        None
    };

    // Clone state handles for the spawned task's finalizer — slot
    // transition Stopping → Idle MUST happen on every exit path including
    // panic, otherwise a same-kind start is permanently rejected.
    // `audio_state_for_finalizer` is a separate Arc clone because
    // `audio_state_clone` is moved into `live_transcription_loop` and
    // is no longer accessible after the loop returns; the finalizer
    // needs its own handle to read the final mic position when closing
    // the dictation window.
    let live_state_for_finalizer = live_state.inner().clone();
    let dictation_owns_mic_for_finalizer = dictation_owns_mic.inner().clone();
    let audio_state_for_finalizer = audio_state.inner().clone();

    // Spawn the task and install `Running` UNDER A SINGLE LOCK ACQUIRE.
    //
    // The race this closes: tokio::spawn returns synchronously, but the
    // spawned future starts polling on whatever worker picks it up —
    // potentially in parallel with the parent. If the parent spawns,
    // releases the lock, and only then re-acquires to install `Running`,
    // a fast-failing loop body (e.g. `SileroVad::new()` returning Err
    // before `audio_state.lock().await` even yields the parent's slot)
    // can run its finalizer and write `slot = Idle` BEFORE the parent's
    // re-acquire resolves. The parent then writes `Running(dead_handle)`
    // over Idle, leaving a slot that rejects every future same-kind start
    // and whose `Stopping` write from `stop_live_transcription` never
    // gets cleared (the task is already gone). Wedge until app restart.
    //
    // Holding the lock across `tokio::spawn` + slot assignment makes the
    // pair atomic from the finalizer's view: the finalizer's
    // `live_state.lock().await` can't acquire until the install commits.
    // `tokio::spawn` is purely synchronous (queues the future + returns
    // a JoinHandle); the install is a struct write. The total lock-hold
    // is microseconds, comparable to any other brief mutation under
    // live_state.
    {
        let mut guard = live_state.lock().await;
        let task_handle = tokio::spawn({
            let ctx_guard = ctx.clone();
            let counters_for_loop = counters.clone();
            let stop_requested_for_loop = Arc::clone(&stop_requested);
            async move {
                let result = AssertUnwindSafe(live_transcription_loop(
                    audio_state_clone,
                    ctx,
                    stop_rx,
                    stop_requested_for_loop,
                    restart_intent_rx,
                    session_wav_state,
                    counters_for_loop,
                ))
                .catch_unwind()
                .await;

                // The scheduler is long-lived now — it outlives this session and
                // is shared with any dictation runtime that may also be holding
                // it. We do *not* shut it down at session end. A panic here just
                // means this session's loop is going down; the scheduler keeps
                // serving any other live runtime (e.g. a dictation that started
                // mid-session). Engine-level shutdown happens via
                // `shutdown_transcription_client`, not here.

                if let Err(panic) = result {
                    error!("live transcription panicked: {:?}", panic);
                    let _ = ctx_guard.app_handle.emit(
                        "live-transcription-status",
                        LiveTranscriptionStatus {
                            phase: LiveTranscriptionPhase::Error,
                            chunks_processed: 0,
                            total_audio_seconds: 0.0,
                            error_message: Some(
                                "live transcription crashed unexpectedly".to_string(),
                            ),
                            session_id: ctx_guard.config.session_id.clone(),
                            effective_start_epoch_ms: None,
                            lag_seconds: None,
                            live_drain_backlog_chunks: 0,
                            live_drain_backlog_seconds: 0.0,
                            source_kind: ctx_guard.config.source_kind,
                        },
                    );
                }

                // Close the dictation mic-ownership window with the actual
                // end position read from the audio manager. The closed window
                // is then visible to the session loop on its next tick (or
                // any future tick — closed windows are durable, so a fast
                // dictation that opens-and-closes between two session polls
                // can't be missed). Done BEFORE the slot transitions to Idle
                // so the session sees the closed window before any racing
                // same-kind dictation start could re-open one.
                if matches!(runtime_source_kind, LiveSourceKind::Dictation) {
                    let end_pos: u64 = {
                        let manager = audio_state_for_finalizer.lock().await;
                        manager
                            .mic_buffer_info()
                            .map(|i| i.samples_written as u64)
                            .unwrap_or(0)
                    };
                    dictation_owns_mic_for_finalizer.close(end_pos);
                }

                // Slot transition: Running/Stopping → Idle. Idempotent — if
                // `stop_live_transcription` already moved Running → Stopping,
                // we land on Stopping; otherwise we land on Running. Either way
                // we drop the runtime here so a fresh same-kind start can pass
                // the `is_idle` guard.
                {
                    let mut slots = live_state_for_finalizer.lock().await;
                    *slots.slot_mut(runtime_source_kind) = RuntimeSlot::Idle;
                }

                // Final step: signal the broker that no live session owns audio
                // state. Session-only — dictation runtimes don't touch this flag.
                if let Some(flag) = live_session_present_for_task {
                    flag.store(false, Ordering::Release);
                }
            }
        });

        *guard.slot_mut(runtime_source_kind) = RuntimeSlot::Running(LiveTranscriptionRuntime {
            controller: LiveTranscriptionController {
                task_handle,
                stop_tx: Some(stop_tx),
                stop_requested,
                session_id: controller_session_id,
                effective_start_epoch_ms,
                counters,
                session_start_instant,
                session_offset_base_seconds,
            },
            vocabulary_hints: vocab_hints,
            source_kind: runtime_source_kind,
            mic_ownership_guard,
        });
    }
    starting_revert_guard.disarm();

    Ok(LiveTranscriptionStartResult {
        effective_start_epoch_ms,
    })
}

#[tauri::command]
#[specta::specta]
pub async fn stop_live_transcription(
    live_state: tauri::State<'_, LiveTranscriptionState>,
    audio_state: tauri::State<'_, AudioManagerState>,
    restart_intent_inbox: tauri::State<'_, RestartIntentInbox>,
    kind: Option<LiveSourceKind>,
) -> Result<(), CommandError> {
    // Tauri/Specta can't fill a missing invoke arg from a Rust `default`
    // — `Option<>` is the only way to keep old callers working during the
    // transition. `None` means session, matching the legacy semantics.
    let target = kind.unwrap_or(LiveSourceKind::Session);
    let mut guard = live_state.lock().await;

    // Clear the restart-intent inbox first (session-only — dictation
    // doesn't use the inbox) so the broker can't post into a soon-to-be-
    // dropped receiver. The loop's select! will hit the stop_rx branch
    // and exit; any drained-but-unprocessed intent is discarded along
    // with the receiver.
    if matches!(target, LiveSourceKind::Session) {
        let mut inbox_guard = restart_intent_inbox
            .inner()
            .lock()
            .expect("restart-intent inbox poisoned");
        *inbox_guard = None;
    }

    // Take the runtime out of `Running` and put it into `Stopping`. The
    // runtime stays in the slot until the spawned task's finalizer
    // transitions to `Idle` — same-kind starts during this window are
    // rejected at `start_live_transcription`.
    let slot = guard.slot_mut(target);
    let mut runtime = match std::mem::replace(slot, RuntimeSlot::Idle) {
        RuntimeSlot::Running(r) => r,
        // Already stopping or idle — same effect either way (idempotent stop).
        RuntimeSlot::Stopping(r) => {
            *slot = RuntimeSlot::Stopping(r);
            return Ok(());
        }
        RuntimeSlot::Starting => {
            *slot = RuntimeSlot::Starting;
            return Err(CommandError::InvalidInput {
                message: "live transcription is still starting; cannot stop yet".into(),
            });
        }
        RuntimeSlot::Idle => {
            *slot = RuntimeSlot::Idle;
            return Err(CommandError::InvalidInput {
                message: format!("live transcription ({}) is not running", target.as_str()),
            });
        }
    };
    runtime
        .controller
        .stop_requested
        .store(true, Ordering::Release);
    let stop_tx = runtime.controller.stop_tx.take();
    *slot = RuntimeSlot::Stopping(runtime);
    drop(guard);

    if let Some(tx) = stop_tx {
        let positions = {
            let manager = audio_state.lock().await;
            stop_positions_with_tail(&manager)
        };
        let _ = tx.send(StopRequest { positions });
    }
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn get_live_transcription_status(
    live_state: tauri::State<'_, LiveTranscriptionState>,
    kind: Option<LiveSourceKind>,
) -> Result<LiveTranscriptionStatus, CommandError> {
    let target = kind.unwrap_or(LiveSourceKind::Session);
    let guard = live_state.lock().await;
    match guard.slot(target).runtime() {
        Some(runtime) => {
            let c = &runtime.controller;
            let (
                chunks_processed,
                total_audio_seconds,
                lag_seconds,
                live_drain_backlog_chunks,
                live_drain_backlog_seconds,
            ) = c.snapshot();
            let phase = if c.is_running() {
                LiveTranscriptionPhase::Running
            } else {
                LiveTranscriptionPhase::Stopped
            };
            Ok(LiveTranscriptionStatus {
                phase,
                chunks_processed,
                total_audio_seconds,
                error_message: None,
                session_id: c.session_id.clone(),
                effective_start_epoch_ms: Some(c.effective_start_epoch_ms),
                lag_seconds,
                live_drain_backlog_chunks,
                live_drain_backlog_seconds,
                source_kind: runtime.source_kind,
            })
        }
        None => Ok(LiveTranscriptionStatus {
            phase: LiveTranscriptionPhase::Stopped,
            chunks_processed: 0,
            total_audio_seconds: 0.0,
            error_message: None,
            session_id: None,
            effective_start_epoch_ms: None,
            lag_seconds: None,
            live_drain_backlog_chunks: 0,
            live_drain_backlog_seconds: 0.0,
            source_kind: target,
        }),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn update_vocabulary_hints(
    live_state: tauri::State<'_, LiveTranscriptionState>,
    hints: String,
) -> Result<(), CommandError> {
    // Vocabulary hints are session-scoped (Whisper initial_prompt for a
    // long recording, not a transient dictation utterance).
    let guard = live_state.lock().await;
    if let Some(runtime) = guard.session.runtime() {
        let mut vocab = runtime.vocabulary_hints.lock().await;
        *vocab = hints;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capture_source_dto_into() {
        let source: CaptureSource = CaptureSourceDto::MicOnly.into();
        assert_eq!(source, CaptureSource::MicOnly);
        let source: CaptureSource = CaptureSourceDto::SystemOnly.into();
        assert_eq!(source, CaptureSource::SystemOnly);
        let source: CaptureSource = CaptureSourceDto::Mixed.into();
        assert_eq!(source, CaptureSource::Mixed);
    }

    // --- VAD state machine tests ---

    /// Shorthand for poll_vad under the Silero-based state machine.
    /// `energy` here is now a *Silero speech probability* in [0, 1], not
    /// an RMS value. Test assertions that previously used 0.005 / 0.05
    /// RMS values are paired with `scale_probability` below which maps
    /// them onto probabilities around the threshold.
    fn poll(state: &mut SourceVadState, probability: Option<f32>) -> VadAction {
        let tuning = VadTuning {
            speech_threshold: 0.5,
            offset_threshold_ratio: 1.0,
            silence_duration: Duration::from_millis(800),
            max_chunk_duration: Duration::from_secs(30),
            poll_interval: Duration::from_millis(300),
            pre_roll: Duration::ZERO,
        };
        poll_vad(state, probability, &tuning)
    }

    #[test]
    fn test_poll_vad_silence_returns_none() {
        let mut state = SourceVadState::new(AudioSourceLabel::Mic, 0, 0, 48000, 1, 0.0);
        // Below threshold while not speaking → None, stays not-speaking
        let action = poll(&mut state, Some(0.10));
        assert!(matches!(action, VadAction::None));
        assert!(!state.is_speaking);
    }

    #[test]
    fn test_poll_vad_speech_onset() {
        let mut state = SourceVadState::new(AudioSourceLabel::Mic, 0, 0, 48000, 1, 0.0);
        // Above threshold while not speaking → transitions to speaking
        let action = poll(&mut state, Some(0.90));
        assert!(matches!(action, VadAction::None));
        assert!(state.is_speaking);
        assert!(state.speech_start_time.is_some());
    }

    #[test]
    fn test_poll_vad_loud_clears_silence_timer() {
        let mut state = SourceVadState::new(AudioSourceLabel::Mic, 0, 0, 48000, 1, 0.0);
        state.is_speaking = true;
        state.speech_start_time = Some(Instant::now());
        state.silence_since = Some(Instant::now()); // had some silence accumulating
                                                    // Above threshold while speaking, below max duration → clears silence_since
        let action = poll(&mut state, Some(0.90));
        assert!(matches!(action, VadAction::None));
        assert!(state.silence_since.is_none());
    }

    #[test]
    fn test_poll_vad_energy_at_exact_threshold() {
        let mut state = SourceVadState::new(AudioSourceLabel::Mic, 0, 0, 48000, 1, 0.0);
        // Exactly at threshold (0.50) while not speaking — uses >=, so SHOULD trigger onset
        let action = poll(&mut state, Some(0.50));
        assert!(matches!(action, VadAction::None));
        assert!(state.is_speaking);
        assert!(state.speech_start_time.is_some());
    }

    #[test]
    fn test_poll_vad_silence_begins() {
        let mut state = SourceVadState::new(AudioSourceLabel::Mic, 0, 0, 48000, 1, 0.0);
        state.is_speaking = true;
        state.speech_start_time = Some(Instant::now());
        // Below threshold while speaking → sets silence_since but not long enough yet
        let action = poll(&mut state, Some(0.10));
        assert!(matches!(action, VadAction::None));
        assert!(state.silence_since.is_some());
    }

    #[test]
    fn test_poll_vad_silence_triggers_chunk() {
        let mut state = SourceVadState::new(AudioSourceLabel::Mic, 0, 0, 48000, 1, 0.0);
        state.is_speaking = true;
        state.speech_start_time = Some(Instant::now());
        // Silence started longer ago than the threshold
        state.silence_since = Some(Instant::now() - Duration::from_secs(2));
        let action = poll(&mut state, Some(0.10));
        assert!(matches!(action, VadAction::Chunk));
    }

    #[test]
    fn test_poll_vad_force_chunk_max_duration() {
        let mut state = SourceVadState::new(AudioSourceLabel::Mic, 0, 0, 48000, 1, 0.0);
        state.is_speaking = true;
        state.speech_start_time = Some(Instant::now() - Duration::from_secs(31));
        // Above threshold while speaking past max_chunk_duration → ForceChunk
        let action = poll(&mut state, Some(0.90));
        assert!(matches!(action, VadAction::ForceChunk));
    }

    #[test]
    fn test_poll_vad_none_energy() {
        // energy = None → always None regardless of state
        let mut state = SourceVadState::new(AudioSourceLabel::Mic, 0, 0, 48000, 1, 0.0);
        let action = poll(&mut state, None);
        assert!(matches!(action, VadAction::None));

        // Also when speaking
        state.is_speaking = true;
        state.speech_start_time = Some(Instant::now());
        let action = poll(&mut state, None);
        assert!(matches!(action, VadAction::None));
    }

    // --- Prompt decay tests ---

    #[test]
    fn test_prompt_decay_clears_after_timeout() {
        let mut sources = vec![SourceVadState::new(
            AudioSourceLabel::Mic,
            0,
            0,
            48000,
            1,
            0.0,
        )];
        let mut prompt = "some prior transcript context".to_string();
        let last_transcription_at = Some(Instant::now() - Duration::from_secs(6));
        let cleared = check_prompt_decay(&mut sources, &mut prompt, 5.0, last_transcription_at);
        assert!(cleared);
        assert!(prompt.is_empty());
    }

    #[test]
    fn test_prompt_decay_no_clear_with_recent_transcription() {
        let mut sources = vec![SourceVadState::new(
            AudioSourceLabel::Mic,
            0,
            0,
            48000,
            1,
            0.0,
        )];
        let mut prompt = "some context".to_string();
        // Transcription just happened — should not decay
        let last_transcription_at = Some(Instant::now());
        let cleared = check_prompt_decay(&mut sources, &mut prompt, 5.0, last_transcription_at);
        assert!(!cleared);
        assert_eq!(prompt, "some context");
    }

    #[test]
    fn test_prompt_decay_no_clear_before_timeout() {
        let mut sources = vec![SourceVadState::new(
            AudioSourceLabel::Mic,
            0,
            0,
            48000,
            1,
            0.0,
        )];
        let mut prompt = "some context".to_string();
        let last_transcription_at = Some(Instant::now() - Duration::from_secs(2));
        let cleared = check_prompt_decay(&mut sources, &mut prompt, 5.0, last_transcription_at);
        assert!(!cleared);
        assert_eq!(prompt, "some context");
    }

    #[test]
    fn test_prompt_decay_disabled_when_zero() {
        let mut sources = vec![SourceVadState::new(
            AudioSourceLabel::Mic,
            0,
            0,
            48000,
            1,
            0.0,
        )];
        let mut prompt = "some context".to_string();
        let last_transcription_at = Some(Instant::now() - Duration::from_secs(60));
        let cleared = check_prompt_decay(&mut sources, &mut prompt, 0.0, last_transcription_at);
        assert!(!cleared);
        assert_eq!(prompt, "some context");
    }

    #[test]
    fn test_prompt_decay_already_empty() {
        let mut sources = vec![SourceVadState::new(
            AudioSourceLabel::Mic,
            0,
            0,
            48000,
            1,
            0.0,
        )];
        let mut prompt = String::new();
        let last_transcription_at = Some(Instant::now() - Duration::from_secs(10));
        let cleared = check_prompt_decay(&mut sources, &mut prompt, 5.0, last_transcription_at);
        assert!(!cleared);
    }

    #[test]
    fn test_prompt_decay_clears_accumulated_text() {
        let mut sources = vec![
            SourceVadState::new(AudioSourceLabel::Mic, 0, 0, 48000, 1, 0.0),
            SourceVadState::new(AudioSourceLabel::System, 0, 0, 48000, 2, 0.0),
        ];
        sources[0].accumulated_text = "mic transcript context".to_string();
        sources[1].accumulated_text = "system transcript context".to_string();
        let mut prompt = "shared prompt".to_string();
        let last_transcription_at = Some(Instant::now() - Duration::from_secs(6));
        let cleared = check_prompt_decay(&mut sources, &mut prompt, 5.0, last_transcription_at);
        assert!(cleared);
        assert!(prompt.is_empty());
        assert!(sources[0].accumulated_text.is_empty());
        assert!(sources[1].accumulated_text.is_empty());
    }

    #[test]
    fn test_prompt_decay_triggers_on_accumulated_text_only() {
        // shared_prompt is empty but accumulated_text is not — should still trigger
        let mut sources = vec![SourceVadState::new(
            AudioSourceLabel::Mic,
            0,
            0,
            48000,
            1,
            0.0,
        )];
        sources[0].accumulated_text = "stale context from before silence".to_string();
        let mut prompt = String::new();
        let last_transcription_at = Some(Instant::now() - Duration::from_secs(6));
        let cleared = check_prompt_decay(&mut sources, &mut prompt, 5.0, last_transcription_at);
        assert!(cleared);
        assert!(sources[0].accumulated_text.is_empty());
    }

    #[test]
    fn test_prompt_decay_no_clear_when_never_transcribed() {
        // No transcription has ever occurred — prompt may have been seeded
        // but last_transcription_at is None → no decay
        let mut sources = vec![SourceVadState::new(
            AudioSourceLabel::Mic,
            0,
            0,
            48000,
            1,
            0.0,
        )];
        sources[0].accumulated_text = "seeded context".to_string();
        let mut prompt = "shared prompt".to_string();
        let cleared = check_prompt_decay(&mut sources, &mut prompt, 5.0, None);
        assert!(!cleared);
        assert_eq!(prompt, "shared prompt");
        assert_eq!(sources[0].accumulated_text, "seeded context");
    }

    // --- Stream health decision helper tests ---

    #[test]
    fn test_should_stall_restart_mic_always_true() {
        // Mic stall should always trigger restart on all platforms
        assert!(should_stall_restart(&AudioSourceLabel::Mic));
    }

    #[test]
    fn test_should_stall_restart_system_platform_dependent() {
        let result = should_stall_restart(&AudioSourceLabel::System);
        if cfg!(target_os = "windows") {
            // On Windows, system stall should NOT trigger restart (WASAPI silence)
            assert!(!result, "system stall restart should be skipped on Windows");
        } else {
            // On other platforms, system stall should trigger restart
            assert!(
                result,
                "system stall restart should be enabled on non-Windows"
            );
        }
    }

    #[test]
    fn test_classify_empty_flush_table() {
        // Table-driven: (source, has_stream_error, expected)
        let cases = [
            // MicOnly is always an error regardless of stream error flag
            (
                CaptureSource::MicOnly,
                false,
                EmptyFlushClassification::Error,
            ),
            (
                CaptureSource::MicOnly,
                true,
                EmptyFlushClassification::Error,
            ),
            // Mixed is always an error (both sources should produce data)
            (CaptureSource::Mixed, false, EmptyFlushClassification::Error),
            (CaptureSource::Mixed, true, EmptyFlushClassification::Error),
            // SystemOnly without stream error: WindowsSilence on Windows, Error elsewhere
            (
                CaptureSource::SystemOnly,
                false,
                if cfg!(target_os = "windows") {
                    EmptyFlushClassification::WindowsSilence
                } else {
                    EmptyFlushClassification::Error
                },
            ),
            // SystemOnly WITH stream error: always Error (real failure)
            (
                CaptureSource::SystemOnly,
                true,
                EmptyFlushClassification::Error,
            ),
        ];

        for (i, (source, has_error, expected)) in cases.iter().enumerate() {
            let result = classify_empty_flush(*source, *has_error);
            assert_eq!(
                result, *expected,
                "case {}: source={:?} has_error={} expected {:?} got {:?}",
                i, source, has_error, expected, result
            );
        }
    }

    // --- VAD tuning guard tests ---

    fn dummy_config() -> LiveTranscriptionConfig {
        LiveTranscriptionConfig {
            silence_duration_ms: 800,
            max_chunk_seconds: 30.0,
            backfill_seconds: 0.0,
            source: CaptureSourceDto::MicOnly,
            mix_config: None,
            language: None,
            prompt_context_chars: None,
            prompt_decay_silence_seconds: None,
            session_id: None,
            persist_audio_part: true,
            audio_save_location: None,
            audio_export_format: None,
            mp3_bitrate: None,
            diarization: false,
            vocabulary_hints: None,
            resume: None,
            source_kind: LiveSourceKind::Session,
        }
    }

    #[test]
    fn test_vad_tuning_whisper_honors_user_silence_duration() {
        // Whisper still honors the user's frontend silence_duration_ms
        // and max_chunk_seconds — only the detector (RMS → Silero) swaps.
        // Threshold values are now Silero probabilities (0.5 / 0.35),
        // shared across engines.
        use yapstack_common::types::EngineKind;
        let mut cfg = dummy_config();
        cfg.silence_duration_ms = 600;
        cfg.max_chunk_seconds = 25.0;
        let t = profile_for(EngineKind::Whisper, &cfg).vad_tuning;
        assert_eq!(t.silence_duration, Duration::from_millis(600));
        assert_eq!(t.max_chunk_duration, Duration::from_secs_f32(25.0));
        assert!((t.speech_threshold - 0.5).abs() < 1e-6);
        // 0.35 / 0.5 = 0.7 hysteresis ratio
        assert!((t.offset_threshold_ratio - 0.7).abs() < 1e-6);
        assert_eq!(t.pre_roll, Duration::ZERO);
    }

    #[test]
    fn test_vad_tuning_parakeet_ignores_user_knobs() {
        // Parakeet tuning is engine-specific backend policy — even if the
        // user sets silence_duration_ms=800 and max_chunk_seconds=30 in the
        // frontend, Parakeet uses its dialogue-aggressive defaults.
        use yapstack_common::types::EngineKind;
        let cfg = dummy_config();
        let t = profile_for(EngineKind::Parakeet, &cfg).vad_tuning;
        assert_eq!(
            t.silence_duration,
            Duration::from_millis(500),
            "Parakeet silence window should be 500ms for meeting-style chunking \
             (FluidAudio uses 750ms; 200ms over-dispatched on fast dialogue and \
             contributed to the stall the cap commit was bounding)"
        );
        assert_eq!(
            t.max_chunk_duration,
            Duration::from_secs(10),
            "Parakeet should force-chunk at 10s of continuous speech"
        );
        assert_eq!(t.poll_interval, Duration::from_millis(100));
        assert_eq!(t.pre_roll, Duration::from_millis(250));
        assert!((t.speech_threshold - 0.5).abs() < 1e-6);
        assert!((t.offset_threshold_ratio - 0.7).abs() < 1e-6);
    }

    // --- Review regression tests ---

    /// Regression: intra-poll speech detection.
    ///
    /// When a poll batch contains speech followed by silence, the VAD state
    /// machine must enter speaking state — not be fooled by only the last
    /// frame's silence probability. This mirrors the live-loop code path
    /// where we now iterate every Silero probability through `poll_vad`.
    #[test]
    fn poll_vad_enters_speaking_within_a_single_poll_batch() {
        use yapstack_common::types::EngineKind;
        let cfg = dummy_config();
        let tuning = profile_for(EngineKind::Parakeet, &cfg).vad_tuning;
        let mut state = SourceVadState::new(AudioSourceLabel::Mic, 0, 0, 48000, 1, 0.0);

        // Simulate one 100 ms Parakeet poll containing: silence → loud →
        // silence. Previously (score_stream returning only the last prob),
        // the live loop would have seen only the trailing silence and the
        // speech onset would have been dropped.
        let probs = [0.10f32, 0.90, 0.10];
        for &p in &probs {
            let _ = poll_vad(&mut state, Some(p), &tuning);
        }

        assert!(
            state.is_speaking,
            "state should have transitioned to speaking during the batch"
        );
        assert!(state.speech_start_time.is_some());
        // The trailing silence frame starts the silence timer but doesn't
        // itself fire Chunk (needs `silence_duration` = 200 ms; only one
        // frame elapsed).
        assert!(state.silence_since.is_some());
    }

    /// Regression: backfill pre-roll must not rewind into a previous chunk.
    ///
    /// Two loud regions separated by a silence gap shorter than
    /// `pre_roll_samples` used to produce overlapping chunks — the second
    /// onset rewound past the first chunk's end. With the
    /// `prev_chunk_end` clamp this no longer happens.
    ///
    /// Exercises the pure state-machine half of the backfill chunker
    /// against a hand-crafted probability sequence so the test isn't
    /// dependent on Silero classifying a synthetic tone as speech.
    #[test]
    fn backfill_chunks_do_not_overlap_when_gap_shorter_than_pre_roll() {
        use yapstack_common::types::EngineKind;
        let cfg = dummy_config();
        let tuning = profile_for(EngineKind::Parakeet, &cfg).vad_tuning;

        let sr: u32 = 48_000;
        // Frame counts derived from the active tuning so this regression
        // doesn't have to be retuned every time the silence/pre-roll
        // defaults shift. We need:
        //   - loud region long enough to be a real chunk (≥ 1 s),
        //   - gap longer than `silence_duration` so the first chunk fires,
        //   - gap shorter than `silence_duration + pre_roll + small slack`
        //     so the second onset's pre-roll could (without the clamp)
        //     reach back into the first chunk's tail.
        let frame_secs = super::super::silero_vad::FRAME_DURATION_SECS;
        let frames_for = |ms: u64| -> usize {
            ((Duration::from_millis(ms).as_secs_f32() / frame_secs).ceil() as usize).max(1)
        };
        let loud_frames = frames_for(1_000); // 1.0 s of speech
                                             // One frame past the silence threshold — guarantees the silence
                                             // break fires while keeping the gap short enough that pre-roll
                                             // would overlap without the clamp.
        let silence_frames = frames_for(tuning.silence_duration.as_millis() as u64);
        let gap_frames = silence_frames + 1;

        let mut probs: Vec<f32> = Vec::with_capacity(loud_frames * 2 + gap_frames);
        probs.extend(std::iter::repeat_n(0.90, loud_frames));
        probs.extend(std::iter::repeat_n(0.10, gap_frames));
        probs.extend(std::iter::repeat_n(0.90, loud_frames));

        let frame_samples = (super::super::silero_vad::FRAME_DURATION_SECS * sr as f32) as usize;
        let total_samples = probs.len() * frame_samples;

        let chunks = backfill_chunks_from_probabilities(&probs, total_samples, sr, &tuning);
        assert!(
            chunks.len() >= 2,
            "expected at least 2 chunks for two loud regions, got {}: {:?}",
            chunks.len(),
            chunks
        );
        for w in chunks.windows(2) {
            assert!(
                w[1].start >= w[0].end,
                "chunks overlap: first ends at {}, next starts at {}",
                w[0].end,
                w[1].start
            );
        }
    }

    #[test]
    fn split_backfill_chunks_bounds_each_scheduler_job() {
        let sr: u32 = 1_000;
        let chunks = split_backfill_chunks(
            vec![VadBackfillChunk {
                start: 0,
                end: 12_500,
            }],
            sr,
        );

        assert_eq!(
            chunks,
            vec![
                VadBackfillChunk {
                    start: 0,
                    end: 5_000,
                },
                VadBackfillChunk {
                    start: 5_000,
                    end: 10_000,
                },
                VadBackfillChunk {
                    start: 10_000,
                    end: 12_500,
                },
            ]
        );
        assert!(chunks.iter().all(|chunk| {
            chunk.end - chunk.start <= (BACKFILL_JOB_QUANTUM_SECS * sr as f32) as usize
        }));
    }

    #[test]
    fn stop_tail_samples_are_frame_aligned() {
        assert_eq!(tail_samples_for(48_000, 2), 28_800);
        assert_eq!(tail_samples_for(16_000, 1), 4_800);
    }

    #[test]
    fn live_drain_backlog_counter_tracks_live_backlog_only() {
        let counters = Arc::new(StdMutex::new(SessionCounters {
            total_chunks: 0,
            total_audio_seconds: 0.0,
            live_drain_backlog_chunks: 0,
            live_drain_backlog_seconds: 0.0,
            total_wall_ms: 0,
            latest_completed_audio_offset_seconds: None,
        }));

        record_live_drain_backlog(&counters, JobOrigin::Backfill, 9.0);
        record_live_drain_backlog(&counters, JobOrigin::Live, 2.5);
        record_live_drain_backlog(&counters, JobOrigin::Live, 0.0);

        let state = counters.lock().expect("counters mutex poisoned");
        assert_eq!(state.live_drain_backlog_chunks, 1);
        assert_eq!(state.live_drain_backlog_seconds, 0.0);
    }

    /// Regression: after backfill extraction, the live VAD must start from
    /// the current write position — not the rewound backfill cursor — so
    /// the live Silero reader doesn't replay backfill samples.
    ///
    /// This is a structural check: we simulate the reset block in
    /// `live_transcription_loop` against a `SourceVadState` built with a
    /// rewound initial position, then verify the fields all advance to
    /// the post-backfill `current` position.
    #[test]
    fn silero_state_resets_to_current_position_after_backfill_extract() {
        let session_start: usize = 1_000; // rewound backfill position
        let current: usize = 50_000; // write_pos after capturing 1s @ 48k mono

        let mut s = SourceVadState::new(
            AudioSourceLabel::Mic,
            session_start,
            session_start,
            48000,
            1,
            0.0,
        );
        // Pre-reset invariants: everything points at the rewound position.
        assert_eq!(s.silero.read_pos, session_start);
        assert_eq!(s.speech_start_pos, session_start);
        assert_eq!(s.earliest_next_chunk_pos, session_start);

        // Apply the same reset the live loop does after backfill extract.
        s.cursor = current;
        s.speech_start_pos = current;
        s.earliest_next_chunk_pos = current;
        s.silero.read_pos = current;
        s.silero.reset();

        assert_eq!(
            s.silero.read_pos, current,
            "Silero read cursor must jump past the backfill window"
        );
        assert_eq!(s.cursor, current);
        assert_eq!(s.speech_start_pos, current);
        assert_eq!(s.earliest_next_chunk_pos, current);
        assert!(s.silero.last_probability.is_none());
    }

    // --- Audio offset anchor (buffer-replacement bookkeeping) ---
    //
    // These tests pin down the invariants the production bug violated.
    // Before the fix, every device-change buffer swap silently rewound
    // chunk offsets by the wall-clock time elapsed since the last reset.
    // See `handle_buffer_replacement` and the field doc on
    // `SourceVadState.audio_offset_anchor_seconds`.

    #[test]
    fn anchor_initializes_from_constructor_arg() {
        // Loop entry / resume sets the anchor to
        // `ctx.session_offset_base_seconds`. Subsequent chunks compute
        // `audio_offset = anchor + samples_since_session_start/sr`, so the
        // first segment after a resume is stamped at `offset_base` (not 0).
        let s = SourceVadState::new(AudioSourceLabel::Mic, 0, 0, 48000, 1, 1720.5);
        assert_eq!(s.audio_offset_anchor_seconds, 1720.5);
    }

    #[test]
    fn reset_for_buffer_replacement_advances_anchor_and_positions() {
        // The reset must move `audio_offset_anchor_seconds` to the caller-
        // computed `new_anchor` *and* point every position cursor at the
        // fresh buffer's `write_pos`. Without the anchor advance, the bug
        // returns: post-swap chunks would carry pre-swap offsets.
        let mut s = SourceVadState::new(AudioSourceLabel::Mic, 0, 0, 24000, 1, 0.0);
        s.dictation_consumed_through = 42; // unrelated, must NOT be touched here
        s.reset_for_buffer_replacement(500, 48000, 2, 367.5);
        assert_eq!(s.audio_offset_anchor_seconds, 367.5);
        assert_eq!(s.session_start_pos, 500);
        assert_eq!(s.cursor, 500);
        assert_eq!(s.speech_start_pos, 500);
        assert_eq!(s.earliest_next_chunk_pos, 500);
        assert_eq!(s.source_sample_rate, 48000);
        assert_eq!(s.source_channels, 2);
        // Mic-ownership cursor reset is the caller's responsibility
        // (handle_buffer_replacement gates it on Session-Mic) so this
        // pure reset must NOT clobber it.
        assert_eq!(s.dictation_consumed_through, 42);
    }

    #[test]
    fn reset_vad_past_preserves_anchor() {
        // Closing a dictation mic-ownership window calls `reset_vad_past`
        // to skip past the window's audio. That's a position rewind only —
        // the buffer-to-session-time mapping is unchanged, so the anchor
        // MUST NOT move. If it did, the bug would re-appear on every
        // dictation hotkey release in a Session run.
        let mut s = SourceVadState::new(AudioSourceLabel::Mic, 0, 0, 48000, 1, 42.5);
        s.is_speaking = true;
        s.reset_vad_past(1000);
        assert_eq!(s.audio_offset_anchor_seconds, 42.5);
        assert_eq!(
            s.session_start_pos, 0,
            "reset_vad_past must not touch session_start_pos"
        );
        assert_eq!(s.cursor, 1000);
        assert!(!s.is_speaking);
    }

    #[test]
    fn chunk_offset_formula_uses_anchor_not_zero() {
        // Pure-formula regression test. The post-fix formula reads the
        // anchor; the pre-fix formula read `session_offset_base_seconds`.
        // With the anchor advanced 367s into a fresh buffer, a chunk
        // extracted at sample_index=48000 (1s into the new buffer at 48 kHz
        // mono) must stamp at 368.0s — not at 1.0s as the bug did.
        let sample_rate: u32 = 48000;
        let channels: u16 = 1;
        let session_start_pos: usize = 0; // fresh buffer post-reset
        let anchor_seconds: f32 = 367.0;
        let extraction_start_pos: usize = 48000; // exactly 1 s of audio after reset
        let samples_since_start = extraction_start_pos.saturating_sub(session_start_pos);
        let audio_offset =
            anchor_seconds + samples_since_start as f32 / (sample_rate as f32 * channels as f32);
        assert!(
            (audio_offset - 368.0).abs() < 1e-3,
            "expected ~368.0s, got {audio_offset}"
        );
    }

    #[test]
    fn two_consecutive_buffer_replacements_anchors_monotonic() {
        // The cascade we saw in production (AirPods connect → settle → format
        // flip) triggers two BufferReplaced events seconds apart. Each must
        // advance the anchor monotonically; the second swap must not lose
        // the wall-clock time accumulated since the first.
        let mut s = SourceVadState::new(AudioSourceLabel::Mic, 0, 0, 48000, 1, 0.0);
        s.reset_for_buffer_replacement(0, 48000, 1, 10.0);
        assert_eq!(s.audio_offset_anchor_seconds, 10.0);
        s.reset_for_buffer_replacement(0, 24000, 1, 15.5);
        assert_eq!(s.audio_offset_anchor_seconds, 15.5);
        assert_eq!(s.source_sample_rate, 24000);
    }

    #[test]
    fn anchor_is_per_source_not_global() {
        // Mic and System ring buffers can be replaced at different
        // instants (the 19:38 cascade replaced System a second before
        // Mic). Anchors live on `SourceVadState`, not on
        // `TranscriptionContext`, so each source's offsets track its
        // own zero-point independently.
        let mut mic = SourceVadState::new(AudioSourceLabel::Mic, 0, 0, 48000, 1, 100.0);
        let mut sys = SourceVadState::new(AudioSourceLabel::System, 0, 0, 48000, 2, 100.0);
        mic.reset_for_buffer_replacement(0, 48000, 1, 110.0);
        sys.reset_for_buffer_replacement(0, 48000, 2, 115.0);
        assert_eq!(mic.audio_offset_anchor_seconds, 110.0);
        assert_eq!(sys.audio_offset_anchor_seconds, 115.0);
    }

    // --- drain_in_flight_for_source ---
    //
    // Buffer replacement must drain only the in-flight task whose source
    // is being reset. Other sources' outcomes that land in the same
    // drain window must be queued for the main loop, not dropped, and not
    // applied to the wrong source. Tests the contract end-to-end against
    // a `FuturesUnordered<ChunkTaskFuture>` with synthetic outcomes.

    fn dummy_outcome(label: AudioSourceLabel, text: &str) -> ChunkTaskOutcome {
        ChunkTaskOutcome {
            source_label: label,
            accumulated_text: text.to_string(),
            sidecar_dead: false,
        }
    }

    fn push_dummy_outcome(
        chunk_tasks: &mut futures_util::stream::FuturesUnordered<ChunkTaskFuture>,
        outcome: ChunkTaskOutcome,
    ) {
        chunk_tasks.push(Box::pin(async move { outcome }));
    }

    #[tokio::test]
    async fn drain_in_flight_applies_only_matching_source() {
        // Source under replacement is Mic; an earlier System chunk
        // happens to land in the same drain window. We must apply only
        // the Mic outcome to the Mic source and queue the System outcome
        // for the main loop.
        let mut mic = SourceVadState::new(AudioSourceLabel::Mic, 0, 0, 48000, 1, 0.0);
        mic.has_in_flight_task = true;
        mic.accumulated_text = String::from("stale");

        let mut chunk_tasks: futures_util::stream::FuturesUnordered<ChunkTaskFuture> =
            futures_util::stream::FuturesUnordered::new();
        // Push System first so its outcome arrives before the Mic one
        // does — FuturesUnordered isn't strictly FIFO, but with already-
        // resolved futures it picks them up in push order. The drain
        // helper must tolerate either order.
        push_dummy_outcome(
            &mut chunk_tasks,
            dummy_outcome(AudioSourceLabel::System, "system fresh"),
        );
        push_dummy_outcome(
            &mut chunk_tasks,
            dummy_outcome(AudioSourceLabel::Mic, "mic fresh"),
        );
        let mut pending_outcomes: Vec<ChunkTaskOutcome> = Vec::new();

        drain_in_flight_for_source(&mut mic, &mut chunk_tasks, &mut pending_outcomes).await;

        assert!(
            !mic.has_in_flight_task,
            "drain must clear the in-flight flag"
        );
        assert_eq!(mic.accumulated_text, "mic fresh");
        assert_eq!(pending_outcomes.len(), 1);
        assert_eq!(pending_outcomes[0].source_label, AudioSourceLabel::System);
        assert_eq!(pending_outcomes[0].accumulated_text, "system fresh");
    }

    // --- Dictation rebase on mic buffer replacement ---
    //
    // These tests pin down P1 behavior: a mic buffer replacement during
    // an active dictation must preserve mic-ownership state (in the new
    // buffer's coordinate space) instead of unwinding the open window.

    #[test]
    fn rebase_drops_closed_and_rebinds_open() {
        // Active dictation + a prior closed dictation in the queue. The
        // rebase drops the closed window (its positions are in the old
        // buffer's coordinate space and can't survive the swap) and
        // rebases the open window's start to the new buffer's write_pos.
        let state = super::super::transcription::DictationOwnsMic::new();
        // Push a prior closed window into the queue.
        state.open(1_000_000);
        state.close(2_000_000);
        // Now open a new dictation, simulating "active mid-swap".
        state.open(3_000_000);
        let result = state.rebase_for_mic_buffer_replacement(42);
        assert_eq!(result.prior_open, Some(3_000_000));
        assert_eq!(result.new_open, Some(42));
        // The previously-closed window {1_000_000, 2_000_000} was
        // drained on rebase.
        assert_eq!(result.dropped_closed.len(), 1);
        assert_eq!(result.dropped_closed[0].start, 1_000_000);
        assert_eq!(result.dropped_closed[0].end, 2_000_000);
        // Snapshot now reflects the rebased open and empty closed list.
        let snap = state.snapshot();
        assert_eq!(snap.open, Some(42));
        assert!(snap.closed.is_empty());
    }

    #[test]
    fn rebase_with_no_open_is_safe() {
        // Mic swap with no dictation in flight: rebase still drops any
        // stale closed windows but doesn't fabricate an open one.
        let state = super::super::transcription::DictationOwnsMic::new();
        state.open(100);
        state.close(200);
        let result = state.rebase_for_mic_buffer_replacement(999);
        assert_eq!(result.prior_open, None);
        assert_eq!(result.new_open, None);
        assert_eq!(result.dropped_closed.len(), 1);
        assert!(state.snapshot().open.is_none());
        assert!(state.snapshot().closed.is_empty());
    }

    #[test]
    fn truncate_at_earliest_dictation_in_range() {
        // The rescue range [100, 1000) contains a closed window starting
        // at 400 and an open window starting at 700. The truncate point
        // must be the earlier of the two (400) so the rescue stops
        // before any dictated content.
        let state = super::super::transcription::DictationOwnsMic::new();
        state.open(400);
        state.close(500);
        state.open(700);
        let rebase = state.rebase_for_mic_buffer_replacement(0);
        let truncate = earliest_dictation_start_in_range(100, 1000, &rebase);
        assert_eq!(truncate, Some(400));
    }

    #[test]
    fn truncate_returns_none_when_no_overlap() {
        // Closed window {2000, 3000} lives past the rescue range
        // [100, 1000); no truncation needed.
        let state = super::super::transcription::DictationOwnsMic::new();
        state.open(2000);
        state.close(3000);
        let rebase = state.rebase_for_mic_buffer_replacement(0);
        let truncate = earliest_dictation_start_in_range(100, 1000, &rebase);
        assert_eq!(truncate, None);
    }

    #[test]
    fn truncate_returns_none_for_empty_range() {
        // Defensive: empty or inverted ranges return None regardless of
        // dictation state.
        let state = super::super::transcription::DictationOwnsMic::new();
        state.open(500);
        let rebase = state.rebase_for_mic_buffer_replacement(0);
        assert_eq!(earliest_dictation_start_in_range(100, 100, &rebase), None);
        assert_eq!(earliest_dictation_start_in_range(500, 100, &rebase), None);
    }

    #[tokio::test]
    async fn drain_in_flight_handles_empty_tasks_gracefully() {
        // Defensive: if `has_in_flight_task` is true but chunk_tasks is
        // somehow empty (programmer error elsewhere), the drain must
        // clear the flag and warn rather than block forever.
        let mut mic = SourceVadState::new(AudioSourceLabel::Mic, 0, 0, 48000, 1, 0.0);
        mic.has_in_flight_task = true;

        let mut chunk_tasks: futures_util::stream::FuturesUnordered<ChunkTaskFuture> =
            futures_util::stream::FuturesUnordered::new();
        let mut pending_outcomes: Vec<ChunkTaskOutcome> = Vec::new();

        drain_in_flight_for_source(&mut mic, &mut chunk_tasks, &mut pending_outcomes).await;

        assert!(!mic.has_in_flight_task);
        assert!(pending_outcomes.is_empty());
    }
}
