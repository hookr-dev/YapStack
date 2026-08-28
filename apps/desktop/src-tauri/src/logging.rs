//! Unified logging: stderr + rotating file on disk + in-memory ring buffer for UI.
//!
//! The desktop app is the single sink for all logs. The sidecar's stderr is
//! already forwarded into this subscriber via
//! `yapstack_transcription::client::stderr_reader_task` (as tracing events with
//! target `yapstack_transcription_sidecar`), so we do not need a separate
//! appender in the sidecar process.
//!
//! PII contract: log call sites must never include transcript text. Filter
//! call sites in `crates/yapstack-transcription-sidecar/src/engines/*` already log only
//! lengths. If you add new call sites, follow the same rule.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{AppHandle, Emitter};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Tauri event name used to stream log entries to the frontend.
pub const LOG_EVENT_NAME: &str = "log://entry";

/// Mirrors the five `tracing::Level` values so the frontend gets a
/// typed union instead of an unchecked string.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "UPPERCASE")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl From<&Level> for LogLevel {
    fn from(level: &Level) -> Self {
        match *level {
            Level::ERROR => LogLevel::Error,
            Level::WARN => LogLevel::Warn,
            Level::INFO => LogLevel::Info,
            Level::DEBUG => LogLevel::Debug,
            Level::TRACE => LogLevel::Trace,
        }
    }
}

/// One log line visible to the frontend. `ts_ms` is unix-epoch milliseconds.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct LogEntry {
    pub ts_ms: i64,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
}

/// Bounded in-memory buffer of recent entries. Oldest entries drop when full.
pub struct LogBuffer {
    inner: Mutex<VecDeque<LogEntry>>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    fn push(&self, entry: LogEntry) {
        let mut guard = self.inner.lock().expect("log buffer mutex poisoned");
        if guard.len() == self.capacity {
            guard.pop_front();
        }
        guard.push_back(entry);
    }

    pub fn snapshot(&self, limit: usize) -> Vec<LogEntry> {
        let guard = self.inner.lock().expect("log buffer mutex poisoned");
        let len = guard.len();
        let take = limit.min(len);
        guard.iter().skip(len - take).cloned().collect()
    }

    /// Poison-tolerant snapshot for the panic path: recovers the inner data even
    /// if the mutex was poisoned by the very panic we are now handling. Never
    /// itself panics, so it is safe to call from a panic hook (a second panic
    /// there would abort the process with no diagnostic — see
    /// [`write_crash_dump`]).
    fn snapshot_recover(&self, limit: usize) -> Vec<LogEntry> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let len = guard.len();
        let take = limit.min(len);
        guard.iter().skip(len - take).cloned().collect()
    }

    pub fn clear(&self) {
        self.inner
            .lock()
            .expect("log buffer mutex poisoned")
            .clear();
    }
}

/// Tracing `Layer` that appends each event into a `LogBuffer` and emits a
/// Tauri event so the frontend can tail logs live.
pub struct RingBufferLayer {
    buffer: Arc<LogBuffer>,
    app: AppHandle,
}

impl RingBufferLayer {
    pub fn new(buffer: Arc<LogBuffer>, app: AppHandle) -> Self {
        Self { buffer, app }
    }
}

/// Field visitor that renders the event's `message` plus any `key=value` kv
/// fields into a single human-readable string. Mirrors `tracing-subscriber`'s
/// default `fmt` layout enough to be familiar without pulling in that machinery.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    extras: String,
}

impl MessageVisitor {
    fn render(mut self) -> String {
        if self.extras.is_empty() {
            self.message
        } else if self.message.is_empty() {
            self.extras.trim_start().to_string()
        } else {
            let _ = write!(self.message, " {}", self.extras.trim_start());
            self.message
        }
    }

    fn append_kv(&mut self, name: &str, value: impl std::fmt::Display) {
        let _ = write!(self.extras, " {name}={value}");
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            self.append_kv(field.name(), format_args!("{value:?}"));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.append_kv(field.name(), value);
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.append_kv(field.name(), value);
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.append_kv(field.name(), value);
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.append_kv(field.name(), value);
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.append_kv(field.name(), value);
    }
}

impl<S> Layer<S> for RingBufferLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let entry = LogEntry {
            ts_ms: chrono::Utc::now().timestamp_millis(),
            level: meta.level().into(),
            target: meta.target().to_string(),
            message: visitor.render(),
        };
        self.buffer.push(entry.clone());
        // Best-effort: frontend may not be mounted yet, or event system down.
        let _ = self.app.emit(LOG_EVENT_NAME, &entry);
    }
}

/// Default per-crate filter. Can be overridden via the `RUST_LOG` env var.
const DEFAULT_FILTER: &str = "info,\
    yapstack_desktop=debug,\
    yapstack_audio=info,\
    yapstack_transcription=debug,\
    yapstack_transcription_sidecar=debug,\
    frontend=debug,\
    heap=info,\
    tao=warn,\
    wry=warn,\
    hyper=warn,\
    reqwest=warn,\
    sqlx=warn";

/// Install the global tracing subscriber with three layers:
///   1. stderr fmt (developer console)
///   2. rolling daily file under `log_dir` (`yapstack.log.YYYY-MM-DD`)
///   3. in-memory ring buffer + Tauri `log://entry` event stream
///
/// ## Crash durability (R7)
/// The rolling daily file layer keeps `tracing_appender::non_blocking` — its
/// worker thread absorbs the high-rate sidecar-stderr bursts (native ORT / Dawn
/// / whisper lines, one `tracing` event per line) off the emitting threads. The
/// catch that cost us the fresh-install-crash diagnosis twice: that worker only
/// flushes its user-space buffer when its `WorkerGuard` is dropped, and under
/// the release profile's `panic = "abort"` a panic aborts the process WITHOUT
/// unwinding or dropping anything — so the tail of the buffer (often including
/// the fatal line) never reaches disk.
///
/// Rather than make every event a synchronous file write (which perturbs the
/// binary enough to trip a latent, memory-layout-sensitive `SQLITE_NOMEM` in the
/// vendored cr-sqlite `crsql_as_crr` path — see the R7 receipt), we close the
/// gap where it matters: a panic hook (see [`install_panic_hook`]) synchronously
/// dumps the in-memory ring buffer — which [`RingBufferLayer`] fills on EVERY
/// event, with no worker-thread delay — plus the panic message and location to a
/// dedicated crash file, `fsync`ing before the process dies. That guarantees a
/// crash leaves the panic AND the full recent history durably on disk even when
/// the `non_blocking` tail is lost. The `WorkerGuard` is still returned and held
/// for the process lifetime (see lib.rs) so clean exits flush normally.
pub fn init(log_dir: &Path, app: AppHandle) -> (Arc<LogBuffer>, WorkerGuard) {
    let file_appender = tracing_appender::rolling::daily(log_dir, "yapstack.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    let buffer = Arc::new(LogBuffer::new(500));

    let stderr_layer = fmt::layer().with_writer(std::io::stderr).with_target(true);
    let file_layer = fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(true);
    let ring_layer = RingBufferLayer::new(buffer.clone(), app);

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .with(ring_layer)
        .try_init();

    // Crash-durability seam: a panic anywhere after this point flushes the ring
    // buffer + the panic to `yapstack-panic.log`, surviving `panic=abort`.
    install_panic_hook(buffer.clone(), log_dir.to_path_buf());

    tracing::info!(log_dir = ?log_dir, "logging initialised");
    (buffer, guard)
}

/// Filename of the dedicated crash log written by the panic hook.
pub const PANIC_LOG_FILENAME: &str = "yapstack-panic.log";

/// Extract a human-readable message from a panic payload. Standard library
/// panics carry either a `&str` (`panic!("literal")`) or a `String`
/// (`panic!("{}", x)`); anything else is opaque. Factored out as a pure
/// function so the extraction is unit-testable without provoking a real panic.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Synchronously append a crash record — a header plus the recent in-memory log
/// history — to `dir/yapstack-panic.log`, flushing to disk before returning.
///
/// This is the R7 durability guarantee's teeth: the ring buffer holds the last
/// N entries synchronously (filled by [`RingBufferLayer`] on every event,
/// independent of the `non_blocking` file worker), so dumping it here leaves the
/// crash context on disk even under `panic=abort`. Deliberately best-effort and
/// panic-free: it uses the poison-tolerant [`LogBuffer::snapshot_recover`] and
/// ignores every I/O error, because a second panic inside a panic hook aborts
/// the process with no diagnostic at all.
fn write_crash_dump(dir: &Path, buffer: &LogBuffer, header: &str) {
    let path = dir.join(PANIC_LOG_FILENAME);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let _ = writeln!(file, "\n===== {header} =====");
    for entry in buffer.snapshot_recover(500) {
        let _ = writeln!(
            file,
            "{} [{:?}] {}: {}",
            entry.ts_ms, entry.level, entry.target, entry.message
        );
    }
    // Flush to the kernel AND request an fsync so the record survives even an
    // immediate process abort.
    let _ = file.flush();
    let _ = file.sync_all();
}

/// Install a panic hook (once) that leaves durable crash evidence before
/// chaining to the previously-installed hook.
///
/// Delivers the R7 guarantee: a panic ANYWHERE after logging init leaves the
/// panic message + the recent log history on disk, even under `panic=abort`
/// where nothing unwinds or drops (the hook still runs — that is how the default
/// abort message is printed). It writes the durable crash dump FIRST (so the
/// evidence lands regardless of what follows), then emits the panic through
/// `tracing` for the stderr layer and normal flow.
fn install_panic_hook(buffer: Arc<LogBuffer>, log_dir: PathBuf) {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(move || {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let location = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "unknown".to_string());
            let message = panic_message(info.payload());
            // Durable first: the ring buffer + this panic, fsync'd to the crash file.
            write_crash_dump(
                &log_dir,
                &buffer,
                &format!("PANIC at {location}: {message}"),
            );
            // Best-effort structured line for the stderr layer / clean-exit flow.
            tracing::error!(target: "panic", location = %location, "panic: {message}");
            previous(info);
        }));
    });
}

/// How often the background sampler logs process memory. 60s strikes a
/// balance between "useful for spotting growth over a multi-hour session"
/// and "not noisy in the rolling log file."
pub const MEMORY_SAMPLE_INTERVAL: Duration = Duration::from_secs(60);

/// Spawn a daemon thread that emits a `tracing` event with the current
/// process-level memory footprint every [`MEMORY_SAMPLE_INTERVAL`]. The
/// returned `JoinHandle` is detached by the caller; the thread runs for
/// the life of the process. Exits silently on platforms where
/// `memory_stats` returns `None`.
///
/// Sampling Rust-side measures the whole process — Rust heap, audio
/// buffers, model weights, AND the WebView — which is the right
/// granularity for "is the app leaking?" triage and is the only path
/// that works on WKWebView (macOS), where `performance.memory` is
/// unavailable. The frontend bridge handles on-demand JS-heap snapshots
/// via `captureDiagnostics`; we don't run a parallel periodic timer
/// there, since process RSS captures the same growth signal.
pub fn spawn_memory_sampler() -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("yapstack-memory-sampler".into())
        .spawn(|| {
            // memory_stats availability is deterministic per-platform, so
            // probe once. If unavailable, log a single warn and exit
            // rather than spinning a thread that no-ops every minute.
            if memory_stats::memory_stats().is_none() {
                tracing::warn!(
                    target: "heap",
                    "process memory_stats unavailable on this platform; periodic sampling disabled"
                );
                return;
            }
            // Emit a baseline immediately so the first session-relative
            // delta is visible without waiting a full interval.
            log_memory_sample("baseline");
            loop {
                thread::sleep(MEMORY_SAMPLE_INTERVAL);
                log_memory_sample("interval");
            }
        })
        .expect("failed to spawn memory sampler thread")
}

fn log_memory_sample(reason: &'static str) {
    if let Some(stats) = memory_stats::memory_stats() {
        // RSS only. `stats.virtual_mem` reports hundreds of GB on macOS
        // 14+ due to kernel-reserved address ranges, which would crowd
        // the log with a meaningless 6-digit number on our primary
        // platform. RSS is the signal we actually care about.
        let phys_mb = stats.physical_mem as f64 / (1024.0 * 1024.0);
        tracing::info!(
            target: "heap",
            "process physMB={phys_mb:.1} reason={reason}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env var that switches [`panic_hook_subprocess_child`] into its "actually
    /// panic" role; its value is the temp log dir the child dumps into. Unset
    /// during a normal test run, so the child test is a no-op then.
    const PANIC_HOOK_SMOKE_ENV: &str = "YAPSTACK_PANIC_HOOK_SMOKE_DIR";
    /// Panic message the subprocess child raises; the parent asserts it lands in
    /// the crash file.
    const SUBPROC_PANIC_MESSAGE: &str = "subprocess boot crash — real panic path";
    /// Ring-buffer line the subprocess child seeds; the parent asserts prior
    /// history survives the crash alongside the panic header.
    const SUBPROC_RING_LINE: &str = "boot-step-before-subprocess-crash";

    /// Panic-payload extraction handles the two shapes std panics actually
    /// produce (`&str` and `String`) plus the opaque fallback. This is the
    /// in-test-provable half of the panic-hook guarantee — the hook's *content*
    /// is correct. (That a `panic=abort` exit still flushes it is proven
    /// by-construction: the crash dump fsyncs the ring buffer, and the kernel
    /// keeps written bytes across a process abort.)
    #[test]
    fn panic_message_extracts_str_string_and_opaque() {
        let s: Box<dyn std::any::Any + Send> = Box::new("literal panic");
        assert_eq!(panic_message(&*s), "literal panic");

        let owned: Box<dyn std::any::Any + Send> = Box::new(String::from("formatted panic"));
        assert_eq!(panic_message(&*owned), "formatted panic");

        let opaque: Box<dyn std::any::Any + Send> = Box::new(42u32);
        assert_eq!(panic_message(&*opaque), "<non-string panic payload>");
    }

    /// The crash-dump seam that makes a `panic=abort` diagnosable: the ring
    /// buffer holds recent lines synchronously, and `write_crash_dump` persists
    /// them + the panic header to the dedicated crash file, fsync'd, with no
    /// guard drop or worker-thread flush. This is the in-test-provable half;
    /// that the fsync'd bytes survive the abort itself is by-construction (the
    /// kernel owns written bytes across a process abort).
    #[test]
    fn crash_dump_persists_ring_buffer_and_panic_header() {
        let dir = tempfile::tempdir().expect("temp dir");
        let buffer = LogBuffer::new(500);
        buffer.push(LogEntry {
            ts_ms: 1,
            level: LogLevel::Info,
            target: "yapstack_desktop".to_string(),
            message: "boot-step-before-crash".to_string(),
        });

        write_crash_dump(
            dir.path(),
            &buffer,
            "PANIC at lib.rs:1:1: simulated boot crash",
        );

        let dumped = std::fs::read_to_string(dir.path().join(PANIC_LOG_FILENAME))
            .expect("crash file must exist");
        assert!(
            dumped.contains("simulated boot crash"),
            "panic header must be in the crash file: {dumped}"
        );
        assert!(
            dumped.contains("boot-step-before-crash"),
            "prior ring-buffer history must be in the crash file: {dumped}"
        );
    }

    /// End-to-end proof of the R7 durability guarantee against the REAL panic
    /// path — not the by-construction hand-wave the sibling tests settle for.
    ///
    /// We re-exec THIS test binary as a child (`current_exe`) that installs the
    /// production panic hook against a temp log dir, seeds the ring buffer, and
    /// then genuinely `panic!`s. The parent asserts the child died abnormally
    /// AND that the hook fsync'd the panic message + ring-buffer history into
    /// `yapstack-panic.log` before the process was torn down — i.e. the crash
    /// dump survives even when nothing unwinds.
    ///
    /// A subprocess is what makes this viable under the `--features sync` test
    /// binary: it statically links the vendored cr-sqlite staticlib, whose
    /// no_std `panic=abort` runtime shadows the unwinding runtime symbols
    /// (`eh_personality` / `__rust_start_panic` — the same family the Windows
    /// `/FORCE:MULTIPLE` link flag exists for). In that binary an in-process
    /// `catch_unwind` of a real panic hits `failed to initiate panic, error 3`
    /// and aborts the whole harness. Containing the abort inside a child process
    /// lets us exercise the true `panic=abort` production path for real, in both
    /// the default and the sync test binaries.
    #[test]
    fn panic_hook_writes_crash_dump_on_real_panic() {
        let dir = tempfile::tempdir().expect("temp dir");
        let exe = std::env::current_exe().expect("current test exe");
        let status = std::process::Command::new(exe)
            .args(["logging::tests::panic_hook_subprocess_child", "--exact"])
            // Child noise (incl. the runtime's abort message) is expected; keep
            // it out of a GREEN gate's output. The assertions carry the proof.
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .env(PANIC_HOOK_SMOKE_ENV, dir.path())
            .status()
            .expect("spawn child test process");

        assert!(
            !status.success(),
            "child must die abnormally on the real panic (status: {status:?})"
        );

        let dumped = std::fs::read_to_string(dir.path().join(PANIC_LOG_FILENAME))
            .expect("panic hook must have fsync'd the crash file before the process died");
        assert!(
            dumped.contains(SUBPROC_PANIC_MESSAGE),
            "panic message must be in the crash file: {dumped}"
        );
        assert!(
            dumped.contains(SUBPROC_RING_LINE),
            "ring-buffer history must be in the crash file: {dumped}"
        );
    }

    /// Subprocess entry point for [`panic_hook_writes_crash_dump_on_real_panic`]
    /// — a test only in the libtest-harness sense, not a standalone assertion.
    ///
    /// During a normal `cargo test` run the env var is unset and this is a
    /// no-op that passes. When the parent re-execs the test binary with
    /// `PANIC_HOOK_SMOKE_ENV` pointing at a temp dir, this installs the REAL
    /// production panic hook, seeds a recognizable ring-buffer line, and triggers
    /// a REAL panic so the hook runs on the genuine `panic=abort` path.
    #[test]
    fn panic_hook_subprocess_child() {
        let Ok(dir) = std::env::var(PANIC_HOOK_SMOKE_ENV) else {
            return; // normal run: no-op.
        };
        let buffer = Arc::new(LogBuffer::new(500));
        buffer.push(LogEntry {
            ts_ms: 1,
            level: LogLevel::Info,
            target: "yapstack_desktop".to_string(),
            message: SUBPROC_RING_LINE.to_string(),
        });
        install_panic_hook(buffer, PathBuf::from(dir));
        // The genuine article: a real panic drives the installed hook, which
        // must fsync the crash dump BEFORE the runtime tears the process down.
        panic!("{SUBPROC_PANIC_MESSAGE}");
    }

    /// The crash dump is panic-hook-safe: it must recover a POISONED ring-buffer
    /// mutex (poisoned by the very panic being handled) instead of re-panicking,
    /// which under `panic=abort` would abort with no diagnostic at all.
    ///
    /// This stays an in-process test gated to the unwinding (non-`sync`) build,
    /// for two independent reasons, both rooted in `panic=abort`:
    ///   1. Constructing a poisoned mutex REQUIRES an unwind (a `MutexGuard`
    ///      only sets the poison flag in its `Drop`, which runs while
    ///      `thread::panicking()`). Under `panic=abort` — including the
    ///      `--features sync` test binary, whose vendored cr-sqlite staticlib
    ///      shadows the unwinding runtime — `catch_unwind` and thread panics
    ///      abort instead of unwinding, so a poisoned mutex cannot be produced
    ///      at all.
    ///   2. For the same reason the branch under test is unreachable in an abort
    ///      build: with no guard drops during a panic, the mutex the panic hook
    ///      re-locks is never poisoned.
    ///
    /// So this property is both constructible and meaningful ONLY in an
    /// unwinding build, which the default `cargo test -p yapstack-desktop` gate
    /// provides; the real abort path is covered by
    /// [`panic_hook_writes_crash_dump_on_real_panic`] above.
    #[cfg(not(feature = "sync"))]
    #[test]
    fn crash_dump_tolerates_poisoned_buffer_mutex() {
        let dir = tempfile::tempdir().expect("temp dir");
        let buffer = Arc::new(LogBuffer::new(500));
        buffer.push(LogEntry {
            ts_ms: 1,
            level: LogLevel::Error,
            target: "panic".to_string(),
            message: "line-in-poisoned-buffer".to_string(),
        });
        // Poison the inner mutex the way a panic-while-locked would.
        let b2 = buffer.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = b2.inner.lock().unwrap();
            panic!("poison the buffer");
        }));
        assert!(buffer.inner.is_poisoned());

        // Must not panic, and must still recover the history.
        write_crash_dump(dir.path(), &buffer, "PANIC after poison");
        let dumped = std::fs::read_to_string(dir.path().join(PANIC_LOG_FILENAME))
            .expect("crash file must exist");
        assert!(dumped.contains("line-in-poisoned-buffer"), "{dumped}");
        assert!(dumped.contains("PANIC after poison"), "{dumped}");
    }
}
