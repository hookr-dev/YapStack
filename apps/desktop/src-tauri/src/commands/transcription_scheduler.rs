//! Transcription scheduler: a single-worker priority queue in front of the
//! sidecar lane.
//!
//! The sidecar processes requests serially on the model side; two concurrent
//! `transcribe_with` calls both land in the sidecar's stdin queue in FIFO
//! order, with no way to prefer fresh live speech over historical backfill.
//! The scheduler fixes that: every transcription is submitted as a `Job` with
//! a priority (`FinalFlush > Dictation > Live > Backfill`). A single worker
//! task picks the highest-priority job available, calls `transcribe_with`,
//! and responds via a per-job oneshot. Within the `Live` priority, mic/system
//! sources alternate in strict round-robin so neither can starve the other
//! during a sustained dual-source session. Backfill is also gated by the
//! per-producer busy bitmask so historical work only enters the non-
//! preemptible sidecar lane while every live producer (session mic, session
//! system, dictation) is idle.
//!
//! Lifetime: the scheduler is constructed once at engine init and lives until
//! engine shutdown. Multiple live runtimes (one session, one dictation) clone
//! `Arc<Scheduler>` and submit into the same worker. `submit` rejects with
//! `SchedulerError::Shutdown` once `shutdown_client` has begun, so racing
//! callers that hold a stale clone don't enqueue work into a dead worker.
//!
//! Sidecar liveness: because the worker is the *sole* caller of
//! `transcribe_with`, a transient sidecar error can be resolved cleanly — the
//! worker owns the only live `Arc<TranscriptionClient>` clone outside of its
//! own brief in-call clone, so `Arc::try_unwrap` succeeds at `respawn()` time.
//!
//! Backfill on stop: the session live loop submits any in-flight speech as
//! `FinalFlush` jobs, then awaits the *backfill submitter task* (the one
//! spawned in `live_transcription.rs::process_backfill`). The submitter
//! awaits each chunk's scheduler response before submitting the next — per-
//! chunk prompt context and in-order segment emission both depend on this
//! serial wait — so the submitter, not the scheduler, is what's actually
//! draining backfill post-stop. The session-level shutdown timeout in the
//! live loop is the real ceiling on backfill drain; `shutdown_client`'s
//! timeout only fires at engine shutdown, not at session end.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::sync::{oneshot, Mutex as TokioMutex, Notify};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

/// Origin of a transcription job — mirrors the `origin` field emitted on
/// `LiveSegmentEvent`. This is the *priority class* of the job (which lane
/// it occupies in the scheduler), not the *routing identity* of the runtime
/// it came from. Dictation runtimes submit `Dictation`-class jobs from their
/// mic chunks but may still emit `FinalFlush`-class jobs at stop time;
/// frontends must route by `source_kind`, never by `origin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobOrigin {
    Live,
    Dictation,
    Backfill,
    FinalFlush,
}

impl JobOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            JobOrigin::Live => "live",
            JobOrigin::Dictation => "dictation",
            JobOrigin::Backfill => "backfill",
            JobOrigin::FinalFlush => "final_flush",
        }
    }
}

/// Producer kind for the multi-producer busy bitmask. Backfill is gated
/// while *any* producer's bit is set. Each producer (session mic loop,
/// session system loop, dictation loop) toggles its own bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyKind {
    LiveMic,
    LiveSystem,
    Dictation,
}

impl BusyKind {
    fn bit(self) -> u8 {
        match self {
            BusyKind::LiveMic => 1 << 0,
            BusyKind::LiveSystem => 1 << 1,
            BusyKind::Dictation => 1 << 2,
        }
    }
}

/// Scheduler lifecycle state. Used both for the worker loop and to reject
/// late `submit()` calls from `Arc<Scheduler>` clones held by racing callers
/// (e.g. a session-stop path racing engine shutdown).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchedulerState {
    Running = 0,
    ShuttingDown = 1,
    Terminal = 2,
}

impl SchedulerState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => SchedulerState::Running,
            1 => SchedulerState::ShuttingDown,
            _ => SchedulerState::Terminal,
        }
    }
}

/// Which audio source a job originated from. The scheduler uses this for
/// round-robin fairness at `Live` priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobSource {
    Mic,
    System,
}

/// Parameters for a single transcription request.
pub struct JobRequest {
    pub origin: JobOrigin,
    pub source: JobSource,
    pub wav_path: PathBuf,
    pub language: Option<String>,
    pub diarization: bool,
}

/// Error from a scheduled job.
#[derive(Debug)]
pub enum SchedulerError {
    Cancelled,
    Shutdown,
    SidecarDead,
    Transcription(String),
}

impl std::fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchedulerError::Cancelled => write!(f, "scheduler shut down before job completed"),
            SchedulerError::Shutdown => write!(f, "scheduler is shut down; submit refused"),
            SchedulerError::SidecarDead => write!(f, "sidecar died and could not be restarted"),
            SchedulerError::Transcription(msg) => write!(f, "transcription failed: {msg}"),
        }
    }
}

impl std::error::Error for SchedulerError {}

/// Result of a completed job.
pub struct JobOutcome {
    pub result: Result<yapstack_transcription::TranscriptionResult, SchedulerError>,
    /// Wall-time the worker spent on this job (round-trip `transcribe_with`
    /// duration). Returned to callers so the existing per-chunk pressure
    /// telemetry keeps its `wall_ms` field honest under the scheduler.
    pub wall_ms: u64,
}

struct JobEnvelope {
    request: JobRequest,
    respond: oneshot::Sender<JobOutcome>,
}

struct SchedulerQueues {
    final_flush: VecDeque<JobEnvelope>,
    live_dictation: VecDeque<JobEnvelope>,
    live_mic: VecDeque<JobEnvelope>,
    live_system: VecDeque<JobEnvelope>,
    backfill: VecDeque<JobEnvelope>,
    /// Last source served at `Live` priority — used for round-robin fairness.
    last_live_source: Option<JobSource>,
}

impl SchedulerQueues {
    fn new() -> Self {
        Self {
            final_flush: VecDeque::new(),
            live_dictation: VecDeque::new(),
            live_mic: VecDeque::new(),
            live_system: VecDeque::new(),
            backfill: VecDeque::new(),
            last_live_source: None,
        }
    }

    /// Pick the next job in priority order. Order:
    /// `FinalFlush > Dictation > Live(mic/system round-robin) > Backfill`.
    /// Backfill is gated while any producer bit is set in `producers_busy`.
    fn pick_next(&mut self, producers_busy: bool) -> Option<JobEnvelope> {
        if let Some(job) = self.final_flush.pop_front() {
            return Some(job);
        }
        if let Some(job) = self.live_dictation.pop_front() {
            return Some(job);
        }
        let try_order = match self.last_live_source {
            Some(JobSource::Mic) => [JobSource::System, JobSource::Mic],
            _ => [JobSource::Mic, JobSource::System],
        };
        for src in try_order {
            let q = match src {
                JobSource::Mic => &mut self.live_mic,
                JobSource::System => &mut self.live_system,
            };
            if let Some(job) = q.pop_front() {
                self.last_live_source = Some(src);
                return Some(job);
            }
        }
        if !producers_busy {
            if let Some(job) = self.backfill.pop_front() {
                return Some(job);
            }
        }
        None
    }

    fn push(&mut self, env: JobEnvelope) {
        match (env.request.origin, env.request.source) {
            (JobOrigin::FinalFlush, _) => self.final_flush.push_back(env),
            (JobOrigin::Dictation, _) => self.live_dictation.push_back(env),
            (JobOrigin::Live, JobSource::Mic) => self.live_mic.push_back(env),
            (JobOrigin::Live, JobSource::System) => self.live_system.push_back(env),
            (JobOrigin::Backfill, _) => self.backfill.push_back(env),
        }
    }

    /// Drain all pending jobs and send `Cancelled` responses.
    fn cancel_all(&mut self) {
        let drain = |q: &mut VecDeque<JobEnvelope>| {
            for env in q.drain(..) {
                let _ = env.respond.send(JobOutcome {
                    result: Err(SchedulerError::Cancelled),
                    wall_ms: 0,
                });
            }
        };
        drain(&mut self.final_flush);
        drain(&mut self.live_dictation);
        drain(&mut self.live_mic);
        drain(&mut self.live_system);
        drain(&mut self.backfill);
    }
}

struct SchedulerInner {
    queues: StdMutex<SchedulerQueues>,
    notify: Notify,
    /// The exclusive client held by the scheduler worker. `None` only during
    /// transient respawn windows and after `shutdown_client` has taken it.
    /// The worker is the sole caller of `transcribe_with`, so Arc uniqueness
    /// is guaranteed at `respawn` time.
    client: TokioMutex<Option<Arc<yapstack_transcription::TranscriptionClient>>>,
    shutdown: AtomicBool,
    /// Lifecycle state; gates `submit`. Stores `SchedulerState as u8`.
    state: AtomicU8,
    /// Per-producer busy bitmask. Backfill is gated while any bit is set.
    /// Bits indexed by `BusyKind::bit()`.
    busy_bits: AtomicU8,
    /// Cached engine kind — populated at construction time from the inner
    /// client and exposed via `engine()` so callers don't need to unwrap the
    /// client to read it. The kind is immutable after construction.
    engine: yapstack_common::types::EngineKind,
}

/// Priority-queue scheduler in front of a single sidecar lane.
pub struct TranscriptionScheduler {
    inner: Arc<SchedulerInner>,
    worker: TokioMutex<Option<JoinHandle<()>>>,
}

/// Default upper bound on how long `shutdown_client` waits for the worker
/// to drain remaining jobs. Large enough to absorb a 5-minute backfill window
/// transcribing on a slow sidecar; bounded so a wedged sidecar can't hang the
/// shutdown path forever. The worker exits as soon as the queue is empty, so
/// the typical drain finishes well under this cap.
pub const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u64 = 300;

impl TranscriptionScheduler {
    /// Create a new scheduler that owns `client` for the rest of the engine's
    /// lifetime. Use `shutdown_client` to drain the worker, take the client
    /// out, and shut it down.
    pub fn new(client: Arc<yapstack_transcription::TranscriptionClient>) -> Arc<Self> {
        let engine = client.engine();
        let inner = Arc::new(SchedulerInner {
            queues: StdMutex::new(SchedulerQueues::new()),
            notify: Notify::new(),
            client: TokioMutex::new(Some(client)),
            shutdown: AtomicBool::new(false),
            state: AtomicU8::new(SchedulerState::Running as u8),
            busy_bits: AtomicU8::new(0),
            engine,
        });

        let worker_inner = inner.clone();
        let worker = tokio::spawn(async move {
            scheduler_worker(worker_inner).await;
        });

        Arc::new(Self {
            inner,
            worker: TokioMutex::new(Some(worker)),
        })
    }

    /// Engine kind the scheduler's client was constructed for. Cached at
    /// construction so callers don't need to unwrap the client.
    pub fn engine(&self) -> yapstack_common::types::EngineKind {
        self.inner.engine
    }

    /// Snapshot of the most-recent `EngineInfo` from the inner client.
    /// Returns `None` if the client hasn't loaded a model yet or if the
    /// scheduler is past its shutdown drain (client taken).
    pub async fn engine_info(&self) -> Option<yapstack_transcription::EngineInfo> {
        let guard = self.inner.client.lock().await;
        guard.as_ref().and_then(|c| c.engine_info())
    }

    /// Submit a job to the scheduler. Returns a receiver that resolves when
    /// the worker completes the job. Returns `Err(SchedulerError::Shutdown)`
    /// immediately if the scheduler has begun shutting down — protects
    /// `Arc<Scheduler>` clones held by racing callers (e.g. a session-stop
    /// path racing engine shutdown) from enqueueing into a dead worker.
    pub fn submit(
        &self,
        request: JobRequest,
    ) -> Result<oneshot::Receiver<JobOutcome>, SchedulerError> {
        let (tx, rx) = oneshot::channel();
        let env = JobEnvelope {
            request,
            respond: tx,
        };
        // Re-check lifecycle state *while holding the queue lock* and only
        // push if still Running. Without this, a thread could read
        // `state == Running` outside the lock, get preempted while
        // `shutdown_client` drains + cancels the queue and transitions
        // Terminal, then resume and push into a dead worker's queue —
        // the returned oneshot would never resolve. `shutdown_client`'s
        // queue drain runs after the state transition, so checking under
        // the queue lock pairs the two transitions atomically from the
        // submitter's perspective.
        let mut queues = self.inner.queues.lock().expect("queue mutex poisoned");
        if SchedulerState::from_u8(self.inner.state.load(Ordering::Acquire))
            != SchedulerState::Running
        {
            return Err(SchedulerError::Shutdown);
        }
        queues.push(env);
        drop(queues);
        self.inner.notify.notify_one();
        Ok(rx)
    }

    /// Toggle a producer's busy bit. Backfill is gated while *any* bit is
    /// set. Each producer (session mic loop, session system loop, dictation
    /// loop) toggles its own bit independently.
    pub fn set_busy(&self, kind: BusyKind, busy: bool) {
        let bit = kind.bit();
        let previous = if busy {
            self.inner.busy_bits.fetch_or(bit, Ordering::AcqRel)
        } else {
            self.inner.busy_bits.fetch_and(!bit, Ordering::AcqRel)
        };
        let was_busy = previous != 0;
        let now_busy = if busy { true } else { (previous & !bit) != 0 };
        if was_busy != now_busy {
            // The "any-producer-busy" gate flipped; wake the worker so it
            // re-evaluates whether backfill is now eligible.
            self.inner.notify.notify_one();
        }
    }

    /// Toggle both session bits (`LiveMic` + `LiveSystem`) together. The
    /// session live loop covers both sources in lockstep, so its busy
    /// signal is naturally coarse-grained at this level.
    pub fn set_live_busy(&self, busy: bool) {
        self.set_busy(BusyKind::LiveMic, busy);
        self.set_busy(BusyKind::LiveSystem, busy);
    }

    /// App-level shutdown: drain the worker, take the inner client, and call
    /// `client.shutdown().await`. The scheduler becomes terminal and any
    /// further `submit` from cloned handles is rejected. Safe to call
    /// multiple times; subsequent calls return `Ok`.
    pub async fn shutdown_client(&self, timeout: Duration) -> Result<(), String> {
        if self.inner.shutdown.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.inner
            .state
            .store(SchedulerState::ShuttingDown as u8, Ordering::Release);
        self.inner.notify.notify_one();

        let worker = {
            let mut guard = self.worker.lock().await;
            guard.take()
        };
        let mut worker_clean = true;
        if let Some(worker) = worker {
            let abort_handle = worker.abort_handle();
            match tokio::time::timeout(timeout, worker).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    warn!(
                        "scheduler worker joined with error during shutdown_client: {} — \
                         dropping client without graceful shutdown",
                        e
                    );
                    worker_clean = false;
                }
                Err(_) => {
                    warn!(
                        "scheduler worker did not exit within {}s during shutdown_client — \
                         aborting; dropping client without graceful shutdown",
                        timeout.as_secs()
                    );
                    abort_handle.abort();
                    worker_clean = false;
                }
            }
        }

        {
            let mut queues = self.inner.queues.lock().expect("queue mutex poisoned");
            queues.cancel_all();
        }

        let arc_client = self.inner.client.lock().await.take();
        let mut shutdown_err: Option<String> = None;
        if worker_clean {
            if let Some(arc_client) = arc_client {
                match Arc::try_unwrap(arc_client) {
                    Ok(client) => {
                        if let Err(e) = client.shutdown().await {
                            shutdown_err = Some(format!("client shutdown failed: {e}"));
                        }
                    }
                    Err(_) => {
                        // Worker exited "cleanly" but a clone is still held
                        // somewhere — should not happen if the scheduler is
                        // the sole long-lived owner. Drop our reference and
                        // log.
                        warn!(
                            "shutdown_client: worker exited cleanly but client Arc still has \
                             other holders; dropping our reference"
                        );
                    }
                }
            } else {
                warn!(
                    "shutdown_client: worker exited cleanly but no client to shut down \
                     (already taken upstream?)"
                );
            }
        } else {
            error!(
                "shutdown_client: worker did not exit cleanly; dropping client without \
                 graceful sidecar shutdown"
            );
            drop(arc_client);
        }
        self.inner
            .state
            .store(SchedulerState::Terminal as u8, Ordering::Release);
        match shutdown_err {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }
}

async fn scheduler_worker(inner: Arc<SchedulerInner>) {
    loop {
        let env = {
            let mut queues = inner.queues.lock().expect("queue mutex poisoned");
            let producers_busy = inner.busy_bits.load(Ordering::Acquire) != 0
                && !inner.shutdown.load(Ordering::Acquire);
            queues.pick_next(producers_busy)
        };

        let Some(env) = env else {
            if inner.shutdown.load(Ordering::Acquire) {
                return;
            }
            inner.notify.notified().await;
            continue;
        };

        let outcome = process_job(&inner, env.request).await;
        let _ = env.respond.send(outcome);
    }
}

async fn process_job(inner: &Arc<SchedulerInner>, request: JobRequest) -> JobOutcome {
    let client_arc = {
        let guard = inner.client.lock().await;
        guard.as_ref().cloned()
    };
    // `inner.client` is held by the worker for the scheduler's lifetime —
    // shutdown_and_return only takes it back after the worker has exited, and
    // respawn_client holds the lock across its take/put-back. Reaching this
    // arm means the invariant was broken upstream.
    let client = client_arc.expect("scheduler client present while worker is running");

    let wall_start = std::time::Instant::now();
    let result = client
        .transcribe_with(
            &request.wav_path,
            request.language.as_deref(),
            request.diarization,
        )
        .await;
    let wall_ms = wall_start.elapsed().as_millis() as u64;

    match result {
        Ok(r) => JobOutcome {
            result: Ok(r),
            wall_ms,
        },
        Err(e) => {
            warn!(
                "scheduler: transcribe failed ({:?}/{:?}): {}",
                request.origin, request.source, e
            );
            // Drop our local Arc clone before attempting respawn so the
            // worker's Arc inside `inner.client` is the only remaining
            // reference. Otherwise `Arc::try_unwrap` fails.
            drop(client);

            let sidecar_dead = !client_is_running(inner).await;
            if sidecar_dead {
                info!("scheduler: sidecar died — attempting single respawn");
                if respawn_client(inner).await {
                    let client_arc = {
                        let guard = inner.client.lock().await;
                        guard.as_ref().cloned()
                    };
                    if let Some(client) = client_arc {
                        let retry_start = std::time::Instant::now();
                        let retry = client
                            .transcribe_with(
                                &request.wav_path,
                                request.language.as_deref(),
                                request.diarization,
                            )
                            .await;
                        return JobOutcome {
                            result: retry.map_err(|e| SchedulerError::Transcription(e.to_string())),
                            wall_ms: wall_ms + retry_start.elapsed().as_millis() as u64,
                        };
                    }
                }
                return JobOutcome {
                    result: Err(SchedulerError::SidecarDead),
                    wall_ms,
                };
            }

            JobOutcome {
                result: Err(SchedulerError::Transcription(e.to_string())),
                wall_ms,
            }
        }
    }
}

async fn client_is_running(inner: &Arc<SchedulerInner>) -> bool {
    let guard = inner.client.lock().await;
    match guard.as_ref() {
        Some(c) => c.is_running(),
        None => false,
    }
}

/// Try to respawn the sidecar. Returns `true` on success. The worker is the
/// only holder of the `Arc<TranscriptionClient>` at this point — its in-call
/// clone was dropped before this is reached — so `Arc::try_unwrap` succeeds.
async fn respawn_client(inner: &Arc<SchedulerInner>) -> bool {
    let mut guard = inner.client.lock().await;
    let Some(arc_client) = guard.take() else {
        return false;
    };
    let Ok(mut client) = Arc::try_unwrap(arc_client) else {
        panic!("scheduler worker is the sole Arc<TranscriptionClient> holder");
    };
    match client.respawn().await {
        Ok(()) => {
            *guard = Some(Arc::new(client));
            info!("scheduler: sidecar respawned successfully");
            true
        }
        Err(e) => {
            error!("scheduler: sidecar respawn failed: {}", e);
            *guard = Some(Arc::new(client));
            false
        }
    }
}

/// Convert from the `AudioSourceLabel` DTO used elsewhere in
/// `live_transcription.rs` into the scheduler's internal `JobSource`.
pub fn source_from_label(label: super::live_transcription::AudioSourceLabel) -> JobSource {
    use super::live_transcription::AudioSourceLabel;
    match label {
        AudioSourceLabel::Mic => JobSource::Mic,
        AudioSourceLabel::System => JobSource::System,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_env(
        origin: JobOrigin,
        source: JobSource,
    ) -> (JobEnvelope, oneshot::Receiver<JobOutcome>) {
        let (tx, rx) = oneshot::channel();
        let env = JobEnvelope {
            request: JobRequest {
                origin,
                source,
                wav_path: PathBuf::from("/tmp/placeholder.wav"),
                language: None,
                diarization: false,
            },
            respond: tx,
        };
        (env, rx)
    }

    #[test]
    fn priority_final_before_dictation_before_live_before_backfill() {
        let mut q = SchedulerQueues::new();
        let (backfill, _rx_b) = fake_env(JobOrigin::Backfill, JobSource::Mic);
        let (live, _rx_l) = fake_env(JobOrigin::Live, JobSource::Mic);
        let (dictation, _rx_d) = fake_env(JobOrigin::Dictation, JobSource::Mic);
        let (final_flush, _rx_f) = fake_env(JobOrigin::FinalFlush, JobSource::Mic);
        q.push(backfill);
        q.push(live);
        q.push(dictation);
        q.push(final_flush);

        let first = q.pick_next(false).unwrap();
        assert!(matches!(first.request.origin, JobOrigin::FinalFlush));
        let second = q.pick_next(false).unwrap();
        assert!(matches!(second.request.origin, JobOrigin::Dictation));
        let third = q.pick_next(false).unwrap();
        assert!(matches!(third.request.origin, JobOrigin::Live));
        let fourth = q.pick_next(false).unwrap();
        assert!(matches!(fourth.request.origin, JobOrigin::Backfill));
        assert!(q.pick_next(false).is_none());
    }

    #[test]
    fn dictation_jumps_an_already_queued_live_job() {
        let mut q = SchedulerQueues::new();
        let (live, _) = fake_env(JobOrigin::Live, JobSource::Mic);
        q.push(live);
        let (dictation, _) = fake_env(JobOrigin::Dictation, JobSource::Mic);
        q.push(dictation);

        let first = q.pick_next(false).unwrap();
        assert!(matches!(first.request.origin, JobOrigin::Dictation));
        let second = q.pick_next(false).unwrap();
        assert!(matches!(second.request.origin, JobOrigin::Live));
    }

    #[test]
    fn dictation_ignores_busy_gate() {
        let mut q = SchedulerQueues::new();
        let (dictation, _) = fake_env(JobOrigin::Dictation, JobSource::Mic);
        q.push(dictation);
        // Even with producers busy, dictation is eligible (it bypasses the
        // backfill-only gate the same way live and final_flush do).
        let first = q.pick_next(true).unwrap();
        assert!(matches!(first.request.origin, JobOrigin::Dictation));
    }

    #[test]
    fn live_round_robin_alternates_sources() {
        let mut q = SchedulerQueues::new();
        for _ in 0..3 {
            let (m, _) = fake_env(JobOrigin::Live, JobSource::Mic);
            let (s, _) = fake_env(JobOrigin::Live, JobSource::System);
            q.push(m);
            q.push(s);
        }

        let mut sequence = Vec::new();
        while let Some(env) = q.pick_next(false) {
            sequence.push(env.request.source);
        }
        assert_eq!(sequence.len(), 6);
        assert_eq!(
            sequence,
            vec![
                JobSource::Mic,
                JobSource::System,
                JobSource::Mic,
                JobSource::System,
                JobSource::Mic,
                JobSource::System,
            ]
        );
    }

    #[test]
    fn live_drains_remaining_when_one_side_empty() {
        let mut q = SchedulerQueues::new();
        for _ in 0..3 {
            let (s, _) = fake_env(JobOrigin::Live, JobSource::System);
            q.push(s);
        }
        let mut count = 0;
        while let Some(env) = q.pick_next(false) {
            assert_eq!(env.request.source, JobSource::System);
            count += 1;
        }
        assert_eq!(count, 3);
    }

    #[test]
    fn final_flush_preempts_even_when_live_queued() {
        let mut q = SchedulerQueues::new();
        for _ in 0..2 {
            let (m, _) = fake_env(JobOrigin::Live, JobSource::Mic);
            q.push(m);
        }
        let (f, _) = fake_env(JobOrigin::FinalFlush, JobSource::System);
        q.push(f);
        let first = q.pick_next(false).unwrap();
        assert!(matches!(first.request.origin, JobOrigin::FinalFlush));
    }

    #[test]
    fn backfill_drains_after_live_even_when_interleaved() {
        // Regression guard: a backfill chunk that arrives before a live chunk
        // must still be served *after* the live chunk, not in submit order.
        let mut q = SchedulerQueues::new();
        let (b1, _) = fake_env(JobOrigin::Backfill, JobSource::Mic);
        q.push(b1);
        let (l1, _) = fake_env(JobOrigin::Live, JobSource::Mic);
        q.push(l1);
        let (b2, _) = fake_env(JobOrigin::Backfill, JobSource::Mic);
        q.push(b2);

        assert!(matches!(
            q.pick_next(false).unwrap().request.origin,
            JobOrigin::Live
        ));
        assert!(matches!(
            q.pick_next(false).unwrap().request.origin,
            JobOrigin::Backfill
        ));
        assert!(matches!(
            q.pick_next(false).unwrap().request.origin,
            JobOrigin::Backfill
        ));
    }

    #[test]
    fn backfill_waits_while_live_busy() {
        let mut q = SchedulerQueues::new();
        let (backfill, _) = fake_env(JobOrigin::Backfill, JobSource::Mic);
        q.push(backfill);

        assert!(q.pick_next(true).is_none());
        assert!(matches!(
            q.pick_next(false).unwrap().request.origin,
            JobOrigin::Backfill
        ));
    }

    #[test]
    fn final_flush_and_live_ignore_live_busy_gate() {
        let mut q = SchedulerQueues::new();
        let (backfill, _) = fake_env(JobOrigin::Backfill, JobSource::Mic);
        let (live, _) = fake_env(JobOrigin::Live, JobSource::Mic);
        let (final_flush, _) = fake_env(JobOrigin::FinalFlush, JobSource::System);
        q.push(backfill);
        q.push(live);
        q.push(final_flush);

        assert!(matches!(
            q.pick_next(true).unwrap().request.origin,
            JobOrigin::FinalFlush
        ));
        assert!(matches!(
            q.pick_next(true).unwrap().request.origin,
            JobOrigin::Live
        ));
        assert!(q.pick_next(true).is_none());
        assert!(matches!(
            q.pick_next(false).unwrap().request.origin,
            JobOrigin::Backfill
        ));
    }

    #[test]
    fn cancel_all_drains_every_bucket() {
        let mut q = SchedulerQueues::new();
        let (final_flush, mut rx_f) = fake_env(JobOrigin::FinalFlush, JobSource::Mic);
        let (dictation, mut rx_d) = fake_env(JobOrigin::Dictation, JobSource::Mic);
        let (live, mut rx_l) = fake_env(JobOrigin::Live, JobSource::System);
        let (backfill, mut rx_b) = fake_env(JobOrigin::Backfill, JobSource::Mic);
        q.push(final_flush);
        q.push(dictation);
        q.push(live);
        q.push(backfill);

        q.cancel_all();

        for rx in [&mut rx_f, &mut rx_d, &mut rx_l, &mut rx_b] {
            let out = rx.try_recv().expect("waiter should have a value");
            assert!(matches!(out.result, Err(SchedulerError::Cancelled)));
        }
    }
}
