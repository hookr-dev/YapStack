use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{oneshot, Mutex as TokioMutex};
use tracing::{debug, error, info, trace, warn, Level};
use yapstack_common::types::{EngineKind, SidecarRequest, SidecarResponse, TranscriptSegment};

use crate::error::TranscriptionError;

type Result<T> = std::result::Result<T, TranscriptionError>;

/// One-shot sender that the reader task uses to deliver the final response
/// for a given request id. Progress messages are dropped by the reader and
/// never reach the waiter, so the oneshot fires exactly once.
type ResponseWaiter = oneshot::Sender<SidecarResponse>;
type PendingMap = Arc<StdMutex<HashMap<u64, ResponseWaiter>>>;

/// Remove ANSI CSI / OSC escape sequences. Tiny state machine — avoids pulling
/// in a dependency for one place that needs it. Single-pass over the chars().
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if ch != '\x1B' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            // CSI: consume until the final byte in @..~ (0x40..=0x7E).
            Some('[') => {
                for c in chars.by_ref() {
                    if matches!(c, '\x40'..='\x7E') {
                        break;
                    }
                }
            }
            // OSC: consume until BEL or `ESC \`.
            Some(']') => {
                let mut prev_esc = false;
                for c in chars.by_ref() {
                    if c == '\x07' || (prev_esc && c == '\\') {
                        break;
                    }
                    prev_esc = c == '\x1B';
                }
            }
            // Other 2-byte escapes or trailing ESC: drop both.
            _ => {}
        }
    }
    out
}

/// Log a sidecar response at DEBUG without exposing user speech — Transcription
/// variants carry full segment text, so we never `Debug`-print the value.
fn log_sidecar_response(response: &SidecarResponse) {
    match response {
        SidecarResponse::Transcription {
            id,
            segments,
            duration_ms,
            ..
        } => debug!(
            id = id,
            segments = segments.len(),
            duration_ms = duration_ms,
            "sidecar response: transcription"
        ),
        SidecarResponse::ModelLoaded {
            id,
            accel,
            model_dir,
        } => debug!(
            id = id,
            accel = accel.as_deref(),
            model_dir = model_dir.as_deref(),
            "sidecar response: model_loaded"
        ),
        SidecarResponse::EngineInfo {
            id,
            accel,
            model_dir,
        } => debug!(
            id = id,
            accel = accel.as_deref(),
            model_dir = model_dir.as_deref(),
            "sidecar response: engine_info"
        ),
        SidecarResponse::Error { id, message } => debug!(
            id = id,
            message = message.as_str(),
            "sidecar response: error"
        ),
        SidecarResponse::Progress { id, percent } => {
            debug!(id = id, percent = percent, "sidecar response: progress")
        }
    }
}

#[cfg(test)]
mod strip_ansi_tests {
    use super::strip_ansi;

    #[test]
    fn strips_sgr_colors() {
        let input = "\x1b[32mINFO\x1b[0m hello \x1b[2mworld\x1b[0m";
        assert_eq!(strip_ansi(input), "INFO hello world");
    }

    #[test]
    fn preserves_plain() {
        assert_eq!(strip_ansi("no escapes here"), "no escapes here");
    }

    #[test]
    fn preserves_unicode() {
        assert_eq!(strip_ansi("héllo — world"), "héllo — world");
    }
}

#[cfg(test)]
mod parse_sidecar_level_tests {
    use super::parse_sidecar_level;
    use tracing::Level;

    #[test]
    fn parses_leveled_sidecar_line() {
        let line =
            "2026-07-07T12:00:00.123456Z  INFO yapstack_transcription_sidecar: sidecar ready";
        assert_eq!(parse_sidecar_level(line), Some(Level::INFO));
    }

    #[test]
    fn parses_warn_and_error() {
        assert_eq!(
            parse_sidecar_level("2026-07-07T12:00:00Z  WARN ort::x: heads up"),
            Some(Level::WARN)
        );
        assert_eq!(
            parse_sidecar_level("2026-07-07T12:00:00Z ERROR foo: boom"),
            Some(Level::ERROR)
        );
    }

    #[test]
    fn native_line_without_level_returns_none() {
        // Dawn / whisper.cpp raw output — no tracing level token up front.
        assert_eq!(
            parse_sidecar_level("whisper_init_state: loading model"),
            None
        );
    }

    #[test]
    fn does_not_match_level_word_deep_in_message() {
        // "INFO" appears only in the message body, past the scanned prefix.
        let line = "2026-07-07T12:00:00Z DEBUG t: user typed the word INFO here";
        assert_eq!(parse_sidecar_level(line), Some(Level::DEBUG));
    }
}

#[derive(Debug, Clone)]
pub struct TranscriptionResult {
    pub text: String,
    pub segments: Vec<TranscriptSegment>,
    pub duration_ms: u64,
}

/// What the sidecar reported about the model it actually loaded —
/// resolved execution provider (after any runtime fallback) and the
/// directory or file the model was loaded from. Both fields are
/// optional because older sidecars don't emit them; a current build
/// always populates them on a successful load.
#[derive(Debug, Clone)]
pub struct EngineInfo {
    pub accel: Option<String>,
    pub model_dir: Option<String>,
}

/// Transcription client that supports **concurrent in-flight requests** to the
/// sidecar. Multiple callers can hold `&TranscriptionClient` and call
/// `transcribe_with(...)` simultaneously — they race on a brief `stdin` lock
/// while writing the JSON request, then each awaits its own per-id oneshot
/// response channel. The sidecar still processes requests serially on the
/// model side; the benefit is that the main loop's VAD polling isn't blocked
/// waiting for a transcribe round-trip before the next source can dispatch.
///
/// Methods that mutate process-level state (`respawn`, `shutdown`) still take
/// `&mut self` because they tear down and rebuild the pipeline.
pub struct TranscriptionClient {
    stdin: TokioMutex<ChildStdin>,
    pending: PendingMap,
    next_id: AtomicU64,
    child: StdMutex<Child>,
    /// Set `true` while the reader task is running. Cleared by the reader
    /// task on exit (stdout EOF / I/O error) so `is_running()` can notice a
    /// dead IPC pipeline even when the child hasn't reported its exit status
    /// yet. The old mpsc-based design could check `response_rx.is_closed()`;
    /// with per-id oneshot routing we need an explicit liveness flag.
    reader_alive: Arc<AtomicBool>,
    /// Most recent engine info from the sidecar's last successful
    /// `model_loaded` response. `Mutex` (not RwLock) because reads are
    /// rare (status surface, telemetry tagging) and writes are even
    /// rarer (once per `load_model`/`spawn`). Cleared on respawn until
    /// the next load completes.
    last_engine_info: Arc<StdMutex<Option<EngineInfo>>>,
    // Stored for auto-restart (respawn)
    sidecar_path: PathBuf,
    engine: EngineKind,
    model_path: PathBuf,
    vad_model_path: Option<PathBuf>,
    sortformer_model_path: Option<PathBuf>,
    coreml_cache_dir: Option<PathBuf>,
}

/// Raw parts returned by sidecar spawn.
struct SidecarParts {
    stdin: ChildStdin,
    pending: PendingMap,
    child: Child,
    reader_alive: Arc<AtomicBool>,
}

impl TranscriptionClient {
    /// Spawns the sidecar process and sets up JSON-line IPC.
    ///
    /// `engine` selects which backend the sidecar instantiates (Whisper or
    /// Parakeet). `vad_model_path` is honored only for Whisper (Silero VAD,
    /// reduces hallucinations). `sortformer_model_path` is honored only for
    /// Parakeet — when set, per-request `diarization: true` populates speaker IDs.
    /// `coreml_cache_dir` is Parakeet+macOS only; persistent cache for compiled
    /// CoreML graphs to avoid the ~5 s recompile on every spawn.
    pub async fn spawn(
        sidecar_path: &Path,
        engine: EngineKind,
        model_path: &Path,
        vad_model_path: Option<&Path>,
        sortformer_model_path: Option<&Path>,
        coreml_cache_dir: Option<&Path>,
    ) -> Result<Self> {
        let parts = Self::spawn_sidecar(
            sidecar_path,
            engine,
            model_path,
            vad_model_path,
            sortformer_model_path,
            coreml_cache_dir,
        )
        .await?;

        Ok(Self {
            stdin: TokioMutex::new(parts.stdin),
            pending: parts.pending,
            next_id: AtomicU64::new(1),
            child: StdMutex::new(parts.child),
            reader_alive: parts.reader_alive,
            last_engine_info: Arc::new(StdMutex::new(None)),
            sidecar_path: sidecar_path.to_path_buf(),
            engine,
            model_path: model_path.to_path_buf(),
            vad_model_path: vad_model_path.map(|p| p.to_path_buf()),
            sortformer_model_path: sortformer_model_path.map(|p| p.to_path_buf()),
            coreml_cache_dir: coreml_cache_dir.map(|p| p.to_path_buf()),
        })
    }

    /// Internal: spawn the sidecar process and return raw parts.
    async fn spawn_sidecar(
        sidecar_path: &Path,
        engine: EngineKind,
        model_path: &Path,
        vad_model_path: Option<&Path>,
        sortformer_model_path: Option<&Path>,
        coreml_cache_dir: Option<&Path>,
    ) -> Result<SidecarParts> {
        info!(
            "spawning sidecar: {} engine={} model={} vad_model={:?} sortformer_model={:?} coreml_cache_dir={:?}",
            sidecar_path.display(),
            engine.as_str(),
            model_path.display(),
            vad_model_path.map(|p| p.display().to_string()),
            sortformer_model_path.map(|p| p.display().to_string()),
            coreml_cache_dir.map(|p| p.display().to_string()),
        );

        let mut cmd = tokio::process::Command::new(sidecar_path);
        cmd.arg("--engine").arg(engine.as_str());
        cmd.arg("--model").arg(model_path);

        if let Some(vad_path) = vad_model_path {
            cmd.arg("--vad-model").arg(vad_path);
        }
        if let Some(s_path) = sortformer_model_path {
            cmd.arg("--sortformer-model").arg(s_path);
        }
        if let Some(c_path) = coreml_cache_dir {
            cmd.arg("--coreml-cache-dir").arg(c_path);
        }

        // On Windows, prevent the sidecar from creating a visible console window.
        #[cfg(target_os = "windows")]
        {
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TranscriptionError::SidecarError("failed to open stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TranscriptionError::SidecarError("failed to open stdout".to_string()))?;
        let stderr = child.stderr.take();

        let pending: PendingMap = Arc::new(StdMutex::new(HashMap::new()));
        let reader_alive = Arc::new(AtomicBool::new(true));

        // Spawn reader task with clones of the pending map + liveness flag
        // so it can route per-id responses and signal exit when stdout closes.
        tokio::spawn(Self::reader_task(
            stdout,
            pending.clone(),
            reader_alive.clone(),
        ));

        // Spawn stderr reader task to forward sidecar logs to tracing
        if let Some(stderr) = stderr {
            tokio::spawn(Self::stderr_reader_task(stderr));
        }

        Ok(SidecarParts {
            stdin,
            pending,
            child,
            reader_alive,
        })
    }

    /// Reads JSON-line responses from the sidecar stdout and dispatches each
    /// to the per-id oneshot waiter registered by the caller of
    /// `transcribe_with` / `load_model`. Progress messages are logged and
    /// dropped (oneshot fires exactly once per request — we only deliver the
    /// final response variant).
    ///
    /// Clears `reader_alive` on exit (EOF or I/O error) so `is_running()` can
    /// detect a dead IPC pipeline even when the child process hasn't reported
    /// its exit status yet.
    async fn reader_task(stdout: ChildStdout, pending: PendingMap, reader_alive: Arc<AtomicBool>) {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();

        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<SidecarResponse>(&line) {
                        Ok(response) => {
                            log_sidecar_response(&response);
                            // Skip Progress — it is informational and doesn't
                            // terminate the waiter's oneshot.
                            if let SidecarResponse::Progress { id, percent } = &response {
                                debug!(id = id, "transcription progress: {:.0}%", percent * 100.0);
                                continue;
                            }
                            let id = response_id(&response);
                            let waiter = {
                                let mut map =
                                    pending.lock().expect("pending-response mutex poisoned");
                                id.and_then(|id| map.remove(&id))
                            };
                            match waiter {
                                Some(tx) => {
                                    // Ignore send error — the waiter may have
                                    // timed out and dropped its receiver.
                                    let _ = tx.send(response);
                                }
                                None => {
                                    warn!(
                                        "sidecar response with no registered waiter (id={:?}) — discarding",
                                        id
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            warn!("failed to parse sidecar response: {}: {}", e, line);
                        }
                    }
                }
                Ok(None) => {
                    info!("sidecar stdout closed");
                    break;
                }
                Err(e) => {
                    error!("error reading sidecar stdout: {}", e);
                    break;
                }
            }
        }
        // Stdout closed — any remaining waiters will never get a response.
        // Dropping the senders wakes them with a `RecvError`, which our
        // helper turns into `TranscriptionError::SidecarError`.
        let mut map = pending.lock().expect("pending-response mutex poisoned");
        map.clear();
        drop(map);
        // Signal that the IPC pipeline is dead so `is_running()` triggers a
        // respawn even if the child process hasn't exited yet.
        reader_alive.store(false, Ordering::Release);
    }

    /// Reads stderr from the sidecar and forwards each line to tracing.
    /// Strips ANSI escape sequences defensively — the sidecar itself disables
    /// ANSI but native libraries (ORT, Dawn, whisper.cpp) may still emit
    /// colored output that would render as boxes in the desktop log UI.
    ///
    /// Forwarding is **level-aware**: the sidecar formats its own lines as
    /// `<timestamp> LEVEL target: message`, so we recover that LEVEL and re-log
    /// at the matching level instead of flattening everything to INFO. That
    /// keeps sidecar WARN/ERROR distinguishable in the desktop log/grep, and
    /// keeps sidecar DEBUG lines at DEBUG. Raw native framework chatter (Dawn,
    /// whisper.cpp) carries no level token and is demoted to DEBUG so it stays
    /// off the default INFO log — the old unconditional `info!` re-log turned
    /// every one of those into an INFO line, amplifying the noise.
    async fn stderr_reader_task(stderr: tokio::process::ChildStderr) {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();

        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let stripped = strip_ansi(&line);
                    if !stripped.trim().is_empty() {
                        const T: &str = "yapstack_transcription_sidecar";
                        match parse_sidecar_level(&stripped) {
                            Some(Level::ERROR) => error!(target: T, "{}", stripped),
                            Some(Level::WARN) => warn!(target: T, "{}", stripped),
                            Some(Level::INFO) => info!(target: T, "{}", stripped),
                            Some(Level::DEBUG) => debug!(target: T, "{}", stripped),
                            Some(Level::TRACE) => trace!(target: T, "{}", stripped),
                            // No recognizable level token = raw native output.
                            None => debug!(target: T, "{}", stripped),
                        }
                    }
                }
                Ok(None) => {
                    debug!("sidecar stderr closed");
                    break;
                }
                Err(e) => {
                    warn!("error reading sidecar stderr: {}", e);
                    break;
                }
            }
        }
    }

    /// Register a fresh oneshot waiter under an allocated request id, send
    /// the serialized request over stdin, and return the receiver. The caller
    /// awaits the receiver with its own timeout; on timeout the caller must
    /// deregister by calling `cancel_pending(id)` so the map doesn't leak.
    async fn dispatch_request(
        &self,
        build: impl FnOnce(u64) -> SidecarRequest,
    ) -> Result<(u64, oneshot::Receiver<SidecarResponse>)> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self
                .pending
                .lock()
                .expect("pending-response mutex poisoned");
            map.insert(id, tx);
        }
        let request = build(id);
        if let Err(e) = self.send_request(&request).await {
            // Failed to write — remove the waiter we just registered.
            self.cancel_pending(id);
            return Err(e);
        }
        Ok((id, rx))
    }

    fn cancel_pending(&self, id: u64) {
        let mut map = self
            .pending
            .lock()
            .expect("pending-response mutex poisoned");
        map.remove(&id);
    }

    /// Sends a request to the sidecar process. Serialises just the JSON write
    /// via the stdin mutex — sub-millisecond hold time, so concurrent
    /// `transcribe_with` callers barely see each other.
    async fn send_request(&self, request: &SidecarRequest) -> Result<()> {
        let mut json = serde_json::to_string(request)?;
        json.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(json.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Await a registered waiter with a timeout. On timeout or reader-task
    /// teardown the pending entry is cleaned up.
    async fn await_response(
        &self,
        id: u64,
        rx: oneshot::Receiver<SidecarResponse>,
        timeout_secs: u64,
    ) -> Result<SidecarResponse> {
        let timeout = tokio::time::Duration::from_secs(timeout_secs);
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_recv_err)) => {
                // Reader task dropped the sender — sidecar stdout closed.
                self.cancel_pending(id);
                Err(TranscriptionError::SidecarError(
                    "sidecar process exited unexpectedly".to_string(),
                ))
            }
            Err(_) => {
                self.cancel_pending(id);
                Err(TranscriptionError::Timeout(timeout_secs))
            }
        }
    }

    /// Engine the sidecar was spawned with. Read-only after construction.
    pub fn engine(&self) -> EngineKind {
        self.engine
    }

    /// Transcribes an audio file. Diarization is off — use
    /// [`Self::transcribe_with`] when you need to control it per-call.
    pub async fn transcribe(
        &self,
        audio_path: &Path,
        language: Option<&str>,
        initial_prompt: Option<&str>,
    ) -> Result<TranscriptionResult> {
        self.transcribe_with(audio_path, language, initial_prompt, false)
            .await
    }

    /// Transcribes an audio file with explicit per-request diarization control.
    /// `diarization: true` is honored only when the sidecar was spawned with
    /// `engine = Parakeet` *and* a Sortformer model path; otherwise the flag
    /// is silently a no-op.
    ///
    /// Safe to call concurrently from multiple tasks holding `&TranscriptionClient`.
    pub async fn transcribe_with(
        &self,
        audio_path: &Path,
        language: Option<&str>,
        initial_prompt: Option<&str>,
        diarization: bool,
    ) -> Result<TranscriptionResult> {
        let language = language.map(String::from);
        let initial_prompt = initial_prompt.map(String::from);
        let audio_path = audio_path.to_path_buf();
        let (id, rx) = self
            .dispatch_request(|id| SidecarRequest::Transcribe {
                id,
                audio_path,
                language,
                initial_prompt,
                single_segment: None,
                diarization,
            })
            .await?;

        // 5 minutes timeout for transcription (long audio files)
        let response = self.await_response(id, rx, 300).await?;

        match response {
            SidecarResponse::Transcription {
                text,
                segments,
                duration_ms,
                ..
            } => Ok(TranscriptionResult {
                text,
                segments,
                duration_ms,
            }),
            SidecarResponse::Error { message, .. } => {
                Err(TranscriptionError::TranscriptionFailed(message))
            }
            _ => Err(TranscriptionError::SidecarError(
                "unexpected response type".to_string(),
            )),
        }
    }

    /// Loads a model into the sidecar process. Safe to call concurrently, but
    /// typically called once at startup. On success caches the resolved
    /// `EngineInfo` (accel + model_dir) for later retrieval via
    /// [`Self::engine_info`].
    pub async fn load_model(&self, model_path: &Path) -> Result<EngineInfo> {
        let model_path = model_path.to_path_buf();
        let (id, rx) = self
            .dispatch_request(|id| SidecarRequest::LoadModel { id, model_path })
            .await?;

        // 60 seconds timeout for model loading
        let response = self.await_response(id, rx, 60).await?;

        match response {
            SidecarResponse::ModelLoaded {
                accel, model_dir, ..
            } => {
                let info = EngineInfo { accel, model_dir };
                *self
                    .last_engine_info
                    .lock()
                    .expect("engine_info mutex poisoned") = Some(info.clone());
                Ok(info)
            }
            SidecarResponse::Error { message, .. } => {
                Err(TranscriptionError::SidecarError(message))
            }
            _ => Err(TranscriptionError::SidecarError(
                "unexpected response type".to_string(),
            )),
        }
    }

    /// Asks the sidecar what model it currently has loaded — used at
    /// initial spawn time when the model was loaded from the `--model`
    /// CLI arg before the IPC loop started, so no `model_loaded` was
    /// ever emitted. Cheap on the sidecar side (cached state read).
    /// Caches the result for later retrieval via [`Self::engine_info`].
    pub async fn query_engine_info(&self) -> Result<EngineInfo> {
        let (id, rx) = self
            .dispatch_request(|id| SidecarRequest::QueryEngineInfo { id })
            .await?;
        let response = self.await_response(id, rx, 10).await?;
        match response {
            SidecarResponse::EngineInfo {
                accel, model_dir, ..
            } => {
                let info = EngineInfo { accel, model_dir };
                *self
                    .last_engine_info
                    .lock()
                    .expect("engine_info mutex poisoned") = Some(info.clone());
                Ok(info)
            }
            SidecarResponse::Error { message, .. } => {
                Err(TranscriptionError::SidecarError(message))
            }
            _ => Err(TranscriptionError::SidecarError(
                "unexpected response type for query_engine_info".to_string(),
            )),
        }
    }

    /// Cached engine info from the most recent successful
    /// `load_model` or `query_engine_info`. `None` until either has
    /// been called and succeeded.
    pub fn engine_info(&self) -> Option<EngineInfo> {
        self.last_engine_info
            .lock()
            .expect("engine_info mutex poisoned")
            .clone()
    }

    /// Sends a shutdown request. Does not wait for the process to exit.
    pub async fn shutdown(&self) -> Result<()> {
        let request = SidecarRequest::Shutdown;
        // Best-effort send; if it fails, the process may already be dead.
        let _ = self.send_request(&request).await;
        Ok(())
    }

    /// Returns whether the sidecar process is likely still running.
    ///
    /// Combines two checks:
    /// 1. Reader task is alive (stdout still open). If the reader task has
    ///    exited because of EOF or an I/O error on the pipe, no response can
    ///    ever come back — even if the child process's exit status hasn't
    ///    been reaped yet, the IPC pipeline is effectively dead.
    /// 2. Child process hasn't exited (via `try_wait`).
    pub fn is_running(&self) -> bool {
        if !self.reader_alive.load(Ordering::Acquire) {
            warn!("sidecar IPC reader task exited — treating client as dead");
            return false;
        }
        let mut child = self.child.lock().expect("child mutex poisoned");
        match child.try_wait() {
            Ok(Some(status)) => {
                warn!("sidecar process exited with status: {}", status);
                false
            }
            Ok(None) => true, // still running
            Err(e) => {
                warn!("failed to check sidecar process status: {}", e);
                // Assume running if we can't check
                true
            }
        }
    }

    /// Kills the current sidecar and spawns a fresh one with the same
    /// model/VAD configuration. The model is loaded eagerly by the sidecar
    /// on startup (via --model arg), so no separate load_model call is needed.
    /// `next_id` is preserved to keep request IDs unique across respawns.
    ///
    /// Takes `&mut self` because it tears down and replaces stdin and the
    /// pending-response map — callers cannot race this with `transcribe_with`.
    pub async fn respawn(&mut self) -> Result<()> {
        // Log the old process's exit status for diagnostics and kill it.
        {
            let mut child = self.child.lock().expect("child mutex poisoned");
            match child.try_wait() {
                Ok(Some(status)) => info!(
                    "respawning sidecar (previous exited with status: {})",
                    status
                ),
                Ok(None) => info!("respawning sidecar (previous still running — will kill)"),
                Err(e) => info!(
                    "respawning sidecar (could not check previous status: {})",
                    e
                ),
            }
            let _ = child.start_kill();
        }

        // Any requests still pending on the old sidecar will never get a
        // response — drop their senders so waiters wake with a SidecarError.
        {
            let mut map = self
                .pending
                .lock()
                .expect("pending-response mutex poisoned");
            map.clear();
        }

        let parts = Self::spawn_sidecar(
            &self.sidecar_path,
            self.engine,
            &self.model_path,
            self.vad_model_path.as_deref(),
            self.sortformer_model_path.as_deref(),
            self.coreml_cache_dir.as_deref(),
        )
        .await?;

        self.stdin = TokioMutex::new(parts.stdin);
        self.pending = parts.pending;
        self.reader_alive = parts.reader_alive;
        *self.child.lock().expect("child mutex poisoned") = parts.child;
        // Reset cached engine info — the new sidecar hasn't loaded yet,
        // so the previous values would be stale.
        *self
            .last_engine_info
            .lock()
            .expect("engine_info mutex poisoned") = None;
        // Don't reset next_id — keep incrementing for unique request IDs
        info!(
            "sidecar respawned successfully (next request id: {})",
            self.next_id.load(Ordering::Relaxed)
        );
        Ok(())
    }
}

/// Best-effort parse of the tracing level a sidecar stderr line carries.
///
/// The sidecar's `tracing_subscriber::fmt` output is `<timestamp> LEVEL
/// target: message`, so the level appears as an early whitespace-delimited
/// token. We scan only the first few tokens (level is token index 1 after the
/// single-token RFC3339 timestamp) to avoid matching the word "INFO" inside a
/// message body. Native library output (Dawn, whisper.cpp) has no such token
/// and returns `None`, so the caller can demote it to DEBUG.
fn parse_sidecar_level(line: &str) -> Option<Level> {
    line.split_whitespace().take(3).find_map(|tok| match tok {
        "ERROR" => Some(Level::ERROR),
        "WARN" => Some(Level::WARN),
        "INFO" => Some(Level::INFO),
        "DEBUG" => Some(Level::DEBUG),
        "TRACE" => Some(Level::TRACE),
        _ => None,
    })
}

/// Extract the request id from any `SidecarResponse` variant. Used by the
/// reader task to route responses to per-id waiters.
fn response_id(response: &SidecarResponse) -> Option<u64> {
    match response {
        SidecarResponse::Transcription { id, .. } => Some(*id),
        SidecarResponse::ModelLoaded { id, .. } => Some(*id),
        SidecarResponse::EngineInfo { id, .. } => Some(*id),
        SidecarResponse::Error { id, .. } => Some(*id),
        SidecarResponse::Progress { id, .. } => Some(*id),
    }
}

impl Drop for TranscriptionClient {
    fn drop(&mut self) {
        // Kill the sidecar process to prevent orphaned processes when the
        // client is dropped without an explicit shutdown() call.
        if let Ok(mut child) = self.child.lock() {
            let _ = child.start_kill();
        }
    }
}
