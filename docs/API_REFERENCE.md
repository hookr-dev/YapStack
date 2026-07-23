# API Reference

Complete public API for all crates.

---

## yapstack-common

### Audio utilities (`audio.rs`)

```rust
/// Resample mono audio using sinc interpolation (via rubato).
/// Returns Cow::Borrowed when from_rate == to_rate (zero-copy).
/// Logs a warning on resampler failure and returns unresampled audio as fallback.
pub fn resample(samples: &[f32], from_rate: u32, to_rate: u32) -> Cow<'_, [f32]>;

/// Converts interleaved multi-channel audio to mono by averaging channels per frame.
/// Mono input (channels <= 1) borrows the input slice (zero-copy via Cow::Borrowed).
pub fn deinterleave_to_mono(samples: &[f32], channels: u16) -> Cow<'_, [f32]>;
```

### Config (`config.rs`)

```rust
pub const DEFAULT_SAMPLE_RATE: u32 = 16000;

pub struct AudioConfig {
    pub capture_history_seconds: f32,  // default: 180.0
}
```

### Types (`types.rs`)

```rust
// Audio domain types
pub enum AudioSource { Microphone, SystemAudio }
pub enum DeviceType { Input, Output }
pub struct AudioDeviceInfo { pub name: String, pub device_type: DeviceType, pub is_default: bool }
pub struct AudioChunk { pub samples: Vec<f32>, pub sample_rate: u32, pub channels: u16 }

// Capture state
pub enum CaptureState { Idle, Capturing, Error }
pub struct CaptureStatus { pub state, pub mic_active, pub system_audio_active, pub error_message }
pub enum CaptureSource { MicOnly, SystemOnly, Mixed }
pub enum PermissionStatus { Granted, Denied, NotDetermined, Unavailable }

// Transcription segments
pub struct TranscriptSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub confidence: f32,
    /// Set by the diarization post-pass (Parakeet + Sortformer). `None` for
    /// Whisper segments and for any pre-diarization Parakeet segment.
    /// Serialised with `skip_serializing_if = "Option::is_none"`.
    pub speaker_id: Option<u8>,
}

/// Which transcription engine the sidecar should instantiate.
pub enum EngineKind { Whisper, Parakeet }
impl EngineKind { pub fn as_str(&self) -> &'static str; }  // "whisper" | "parakeet"

// Sidecar IPC protocol (tagged JSON unions)
pub enum SidecarRequest {
    Transcribe {
        id: u64,
        audio_path: PathBuf,
        language: Option<String>,
        initial_prompt: Option<String>,   // Whisper only; Parakeet TDT decoder ignores
        single_segment: Option<bool>,
        diarization: bool,                // Parakeet only when sidecar has --sortformer-model
    },
    LoadModel { id: u64, model_path: PathBuf },
    Shutdown,
}

pub enum SidecarResponse {
    Transcription { id: u64, text: String, segments: Vec<TranscriptSegment>, duration_ms: u64 },
    ModelLoaded { id: u64 },
    Error { id: u64, message: String },
    Progress { id: u64, percent: f32 },
}
```

### Engine catalogue (`engines.rs`)

Source of truth for engine capabilities + supported languages, used by both sidecar validation and the Tauri layer (which exposes it to the frontend via `get_engine_catalogue`).

```rust
pub struct EngineDescriptor {
    pub kind: EngineKind,
    pub display_name: &'static str,
    /// BCP-47 / ISO-639-1 codes. First entry is the engine's primary language.
    pub languages: &'static [&'static str],
    pub supports_diarization: bool,    // Parakeet only
    pub supports_initial_prompt: bool, // Whisper only
}

impl EngineDescriptor {
    pub fn supports_language(&self, code: &str) -> bool;
}

pub fn engine_catalogue() -> &'static [EngineDescriptor];
pub fn descriptor(kind: EngineKind) -> &'static EngineDescriptor;
```

Whisper exposes 99 ISO-639 codes; Parakeet TDT v3 exposes 25 European codes.

---

## yapstack-audio

### Re-exports (`lib.rs`)

```rust
pub use capture::{BoundedExtraction, BufferPositions, SeparateExtraction};
pub use error::AudioError;
pub use export::SessionWavWriter;
pub use manager::AudioManager;
pub use mixer::MixConfig;
pub use ring_buffer::{AudioRangeSnapshot, AudioRingBuffer, RingBufferInfo, SharedAudioRingBuffer};

/// The actual stream configuration used by a device after negotiation.
pub struct DeviceStreamConfig {
    pub sample_rate: u32,
    pub channels: u16,
}
```

### AudioRingBuffer (`ring_buffer.rs`)

```rust
pub type SharedAudioRingBuffer = Arc<AudioRingBuffer>;

pub struct AudioRingBuffer { /* UnsafeCell<Box<[f32]>>, AtomicUsize write_pos, ... */ }

impl AudioRingBuffer {
    // Construction
    pub fn new(capacity: usize, sample_rate: u32, channels: u16) -> Self;
    pub fn with_duration(duration_seconds: f32, sample_rate: u32, channels: u16) -> Self;

    // Properties
    pub fn capacity(&self) -> usize;
    pub fn sample_rate(&self) -> u32;
    pub fn channels(&self) -> u16;
    pub fn samples_written(&self) -> usize;  // monotonic counter, Acquire ordering
    pub fn info(&self) -> RingBufferInfo;

    // Producer API (zero-alloc, non-blocking — safe for audio callbacks)
    pub fn write(&self, data: &[f32]);
    pub fn write_i16(&self, data: &[i16]);   // converts via stack scratch buffer
    pub fn write_u16(&self, data: &[u16]);   // converts via stack scratch buffer

    // Consumer API (allocates Vec<f32>)
    pub fn snapshot(&self, duration_seconds: f32) -> Vec<f32>;
    pub fn snapshot_samples(&self, num_samples: usize) -> Vec<f32>;
    pub fn snapshot_all(&self) -> Vec<f32>;
    pub fn snapshot_since(&self, since_pos: usize) -> Vec<f32>;  // clamped to capacity
    pub fn snapshot_range(&self, from_pos: usize, until_pos: usize) -> AudioRangeSnapshot;  // bounded, reports overrun

    // Energy (zero-allocation RMS computation directly on ring buffer)
    pub fn rms_energy_since(&self, since_pos: usize, max_samples: usize) -> Option<f32>;

    // Reset (only when no concurrent writer)
    pub fn reset(&self);
}

pub struct RingBufferInfo {
    pub capacity_samples: usize,
    pub samples_written: usize,
    pub available_samples: usize,
    pub capacity_seconds: f32,
    pub available_seconds: f32,
    pub sample_rate: u32,
    pub channels: u16,
}

pub struct AudioRangeSnapshot {
    pub samples: Vec<f32>,
    pub start_pos: usize,
    pub end_pos: usize,
    pub overrun: bool,
}
```

### AudioManager (`manager.rs`)

```rust
pub struct AudioManager { /* config, mic, system, state, buffers, session_mark */ }

impl AudioManager {
    // Construction
    pub fn new() -> Self;
    pub fn with_config(config: AudioConfig) -> Self;

    // Capture lifecycle
    pub fn start_capture(&mut self, source: CaptureSource, mic_device_id: Option<&str>) -> Result<()>;
    pub fn start_mic(&mut self, device_name: Option<&str>) -> Result<()>;
    pub fn start_system_audio(&mut self) -> Result<()>;
    pub fn start_all(&mut self, mic_device_id: Option<&str>) -> Result<()>;
    pub fn stop_all(&mut self) -> Result<()>;

    // Status
    pub fn status(&self) -> CaptureStatus;
    pub fn check_system_audio_permission(&self) -> PermissionStatus;

    // Snapshots (returns None if buffer not initialized)
    pub fn snapshot_mic(&self, duration_seconds: f32) -> Option<Vec<f32>>;
    pub fn snapshot_system(&self, duration_seconds: f32) -> Option<Vec<f32>>;
    pub fn snapshot_mic_all(&self) -> Option<Vec<f32>>;
    pub fn snapshot_system_all(&self) -> Option<Vec<f32>>;
    pub fn mic_buffer_info(&self) -> Option<RingBufferInfo>;
    pub fn system_buffer_info(&self) -> Option<RingBufferInfo>;

    // Buffer access
    pub fn mic_buffer(&self) -> Option<&SharedAudioRingBuffer>;
    pub fn system_buffer(&self) -> Option<&SharedAudioRingBuffer>;

    // Position-based extraction (the only capture API; production capture goes
    // through `start_live_transcription`'s ring-buffer-driven loop).
    pub fn buffer_positions(&self) -> BufferPositions;  // current write positions for both buffers
    pub fn mic_write_pos(&self) -> usize;   // current mic buffer write position (0 if no buffer)
    pub fn system_write_pos(&self) -> usize; // current system buffer write position (0 if no buffer)
    pub fn output_sample_rate_for(&self, source: CaptureSource) -> u32;
    pub fn extract_since(&self, positions: &BufferPositions, source: CaptureSource, mix_config: Option<&MixConfig>) -> Option<(Vec<f32>, u32, BufferPositions)>;
    pub fn extract_since_until(&self, positions: &BufferPositions, limits: &BufferPositions, source: CaptureSource, mix_config: Option<&MixConfig>) -> Option<BoundedExtraction>;  // bounded to stop_positions, reports overrun
    pub fn extract_sources_since(&self, positions: &BufferPositions) -> Option<SeparateExtraction>;  // per-source extraction (no mixing)
    pub fn peek_energy_rms(&self, positions: &BufferPositions, duration_secs: f32) -> (Option<f32>, Option<f32>);  // zero-alloc RMS via ring_buffer.rms_energy_since()

    // Stream health
    pub fn mic_has_stream_error(&self) -> bool;      // delegates to MicrophoneCapture
    pub fn system_has_stream_error(&self) -> bool;   // delegates to SystemAudioCapture

    // Stream restart (reuses existing ring buffer — no audio data lost)
    pub fn restart_mic(&mut self, device_name: Option<&str>) -> Result<()>;
    pub fn restart_system_audio(&mut self) -> Result<()>;

    // Config
    pub fn set_config(&mut self, config: AudioConfig);
    pub fn config(&self) -> &AudioConfig;
    pub fn clear_buffers(&mut self);
}
```

### Device (`device.rs`)

```rust
pub fn list_input_devices() -> Result<Vec<AudioDeviceInfo>>;
pub fn list_output_devices() -> Result<Vec<AudioDeviceInfo>>;
pub fn list_all_devices() -> Result<Vec<AudioDeviceInfo>>;
pub fn default_input_device() -> Result<AudioDeviceInfo>;
pub(crate) fn resolve_input_device(name: Option<&str>) -> Result<cpal::Device>;
```

### Microphone (`mic.rs`)

```rust
pub struct MicrophoneCapture { /* stream, is_running, stream_error */ }

impl MicrophoneCapture {
    pub fn new() -> Self;
    pub fn query_device_config(device_name: Option<&str>) -> Result<DeviceStreamConfig>;  // static, no capture
    pub fn start(&mut self, device_name: Option<&str>, buffer: Arc<AudioRingBuffer>) -> Result<()>;
    pub fn stop(&mut self) -> Result<()>;
    pub fn is_running(&self) -> bool;
    pub fn has_stream_error(&self) -> bool;  // true if cpal error callback fired
}
```

### System Audio (`system/mod.rs`)

```rust
pub struct SystemAudioCapture { /* stream, is_running, stream_error */ }

impl SystemAudioCapture {
    pub fn new() -> Self;
    pub fn query_device_config() -> Result<DeviceStreamConfig>;  // static, no capture
    pub fn start(&mut self, buffer: Arc<AudioRingBuffer>) -> Result<()>;
    pub fn stop(&mut self) -> Result<()>;
    pub fn is_available(&self) -> bool;  // true on macOS
    pub fn is_running(&self) -> bool;
    pub fn check_permission(&self) -> PermissionStatus;
    pub fn has_stream_error(&self) -> bool;  // true if cpal error callback fired
}
```

### Mixer (`mixer.rs`)

```rust
pub struct MixConfig {
    pub mic_gain: f32,      // 0.0-1.0, default 0.5
    pub system_gain: f32,   // 0.0-1.0, default 0.5
    pub normalize: bool,    // default true
}

pub(crate) fn deinterleave_to_mono(samples: &[f32], channels: u16) -> Vec<f32>;  // delegates to yapstack_common::audio
pub fn mix_to_mono(mic: &[f32], system: &[f32], config: &MixConfig) -> Vec<f32>;  // both inputs must already be mono
pub fn apply_gain(samples: &[f32], gain: f32) -> Vec<f32>;
pub fn normalize_in_place(samples: &mut [f32]) -> f32;  // returns scaling factor
```

Note: The canonical `deinterleave_to_mono` is in `yapstack_common::audio`. The mixer version is `pub(crate)` and delegates to it.

### Export (`export.rs`)

```rust
pub fn write_wav(samples: &[f32], sample_rate: u32, channels: u16, path: &Path) -> Result<()>;
pub fn write_wav_to_temp(samples: &[f32], sample_rate: u32, channels: u16) -> Result<PathBuf>;
// Output: 16-bit signed PCM. f32 clamped to [-1.0, 1.0] before i16 conversion.
// Temp files use prefix "yapstack_capture_", suffix ".wav", persisted (caller must clean up).

/// Incremental streaming WAV writer for long sessions.
pub struct SessionWavWriter { /* WavWriter<BufWriter<File>>, path, sample_rate, samples_written */ }

impl SessionWavWriter {
    pub fn new(path: PathBuf, sample_rate: u32) -> Result<Self>;  // mono 16-bit PCM
    pub fn write_samples(&mut self, samples: &[f32]) -> Result<()>;  // append f32→i16
    pub fn finalize(self) -> Result<(PathBuf, f32)>;  // flush + update header, returns (path, duration_seconds)
    pub fn duration_seconds(&self) -> f32;
}
```

### Capture Types (`capture.rs`)

```rust
/// Lightweight cursor tracking for both ring buffers. Used by live
/// transcription to track read positions across polls.
pub struct BufferPositions {  // Default
    pub mic_pos: usize,
    pub system_pos: usize,
}

/// Per-source extraction from both ring buffers (no mixing).
pub struct SeparateExtraction {
    pub mic: Option<(Vec<f32>, u32)>,      // (mono_samples, sample_rate)
    pub system: Option<(Vec<f32>, u32)>,   // (mono_samples, sample_rate)
    pub new_positions: BufferPositions,
}

/// Stop-safe bounded extraction result. Samples never include audio written
/// after the supplied limit positions.
pub struct BoundedExtraction {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub new_positions: BufferPositions,
    pub overrun: bool,
}
```

### AudioError (`error.rs`)

```rust
pub enum AudioError {
    DeviceInit(String),
    Capture(String),
    UnsupportedFormat(String),
    NoDevicesAvailable,
    DeviceNotFound(String),
    StreamBuild(String),
    PermissionDenied(String),
    PlatformNotSupported,
    AlreadyRunning,
    NotRunning,
    InvalidBufferConfig(String),
    WavExport(String),
    NoBufferAvailable,
    Mp3Encode(String),
    InvalidBitrate(u16),
}
// From impls: cpal errors, hound::Error, std::io::Error
```

---

## yapstack-transcription

### Re-exports (`lib.rs`)

```rust
pub use client::{TranscriptionClient, TranscriptionResult};
pub use error::TranscriptionError;
pub use model::{ModelInfo, ModelManager, ModelSize, ParakeetVariant, SortformerVariant};
```

### ModelManager (`model.rs`)

Manages **three** model families: Whisper (single ggml file), Parakeet TDT (multi-file ONNX bundle in a per-variant subdirectory), and Sortformer (single ONNX file for diarization). All from HuggingFace with streaming SHA-256 verification.

```rust
pub enum ModelSize { Tiny, Base, Small, Medium }  // Whisper, Serialize + Deserialize
pub enum ParakeetVariant { TdtV3 }                // multilingual, 25 European languages
pub enum SortformerVariant { V2_1 }               // up to 4 speakers

impl ModelSize { /* filename, approximate_size_bytes, download_url, display_name, all */ }
impl ParakeetVariant {
    pub fn dir_name(&self) -> &'static str;                                    // "parakeet-tdt-v3"
    pub fn files(&self) -> &'static [(&'static str, &'static str, u64)];       // (name, url, size) per file
    pub fn approximate_size_bytes(&self) -> u64;
    pub fn display_name(&self) -> &'static str;                                // "Parakeet TDT v3 (~600 MB)"
    pub fn all() -> &'static [ParakeetVariant];
}
impl SortformerVariant { /* filename, download_url, approximate_size_bytes, display_name */ }

pub struct ModelInfo { /* size, downloaded, path, display_name, approximate_size_bytes */ }

pub struct ModelManager { /* models_dir: PathBuf */ }

impl ModelManager {
    pub fn new(app_data_dir: PathBuf) -> Self;        // stored in app_data_dir/models/
    pub fn models_dir(&self) -> &Path;

    // ---- Whisper ----
    pub fn is_available(&self, size: ModelSize) -> bool;
    pub fn model_path(&self, size: ModelSize) -> Option<PathBuf>;
    pub fn expected_model_path(&self, size: ModelSize) -> PathBuf;
    pub async fn download(&self, size: ModelSize, on_progress: impl Fn(f32) + Send) -> Result<PathBuf>;
    pub async fn verify_checksum(&self, size: ModelSize, expected_sha256: &str) -> Result<bool>;
    pub async fn delete(&self, size: ModelSize) -> Result<()>;
    pub fn list_downloaded(&self) -> Vec<ModelSize>;
    pub fn list_all(&self) -> Vec<ModelInfo>;

    // ---- Whisper VAD (Silero ~885KB, auto-downloaded) ----
    pub fn vad_model_path(&self) -> Option<PathBuf>;
    pub async fn download_vad_model(&self, on_progress: impl Fn(f32) + Send) -> Result<PathBuf>;
    pub async fn ensure_vad_model(&self) -> Result<PathBuf>;

    // ---- Parakeet (multi-file ONNX bundle in models_dir/parakeet-<variant>/) ----
    pub fn parakeet_model_dir(&self, variant: ParakeetVariant) -> PathBuf;
    pub fn parakeet_is_available(&self, variant: ParakeetVariant) -> bool;     // all required files present
    pub async fn download_parakeet(&self, variant: ParakeetVariant, on_progress: impl Fn(f32) + Send + Sync) -> Result<PathBuf>;
    pub async fn ensure_parakeet(&self, variant: ParakeetVariant) -> Result<PathBuf>;
    pub async fn delete_parakeet(&self, variant: ParakeetVariant) -> Result<()>;

    // ---- Sortformer (speaker diarization) ----
    pub fn sortformer_model_path(&self, variant: SortformerVariant) -> Option<PathBuf>;
    pub async fn download_sortformer(&self, variant: SortformerVariant, on_progress: impl Fn(f32) + Send) -> Result<PathBuf>;
    pub async fn ensure_sortformer(&self, variant: SortformerVariant) -> Result<PathBuf>;
    pub async fn delete_sortformer(&self, variant: SortformerVariant) -> Result<()>;
}
```

### TranscriptionClient (`client.rs`, was `whisper.rs`)

Engine-agnostic JSON-line IPC client for the sidecar. Spawns the sidecar with `--engine whisper|parakeet` and forwards engine-specific flags (`--vad-model` for Whisper, `--sortformer-model` and `--coreml-cache-dir` for Parakeet).

```rust
pub struct TranscriptionResult {
    pub text: String,
    pub segments: Vec<TranscriptSegment>,  // includes speaker_id when diarization was on
    pub duration_ms: u64,
}

pub struct TranscriptionClient { /* stdin, response_rx, next_id, _child, engine, paths */ }

impl TranscriptionClient {
    pub async fn spawn(
        sidecar_path: &Path,
        engine: EngineKind,
        model_path: &Path,
        vad_model_path: Option<&Path>,           // Whisper only
        sortformer_model_path: Option<&Path>,    // Parakeet only — enables per-call diarization
        coreml_cache_dir: Option<&Path>,         // Parakeet+CoreML only — persistent compile cache
    ) -> Result<Self>;

    pub fn engine(&self) -> EngineKind;          // read-only after construction

    pub async fn transcribe(&mut self, audio_path: &Path, language: Option<&str>, initial_prompt: Option<&str>) -> Result<TranscriptionResult>;
    pub async fn transcribe_with(&mut self, audio_path: &Path, language: Option<&str>, initial_prompt: Option<&str>, diarization: bool) -> Result<TranscriptionResult>;
    pub async fn load_model(&mut self, model_path: &Path) -> Result<()>;
    pub async fn shutdown(&mut self) -> Result<()>;
    pub fn is_running(&self) -> bool;
    pub async fn respawn(&mut self) -> Result<()>; // re-spawns sidecar preserving engine + all paths + next_id
}
// Timeouts: transcribe = 300s, load_model = 60s
```

### TranscriptionError (`error.rs`)

```rust
pub enum TranscriptionError {
    ModelNotFound(String),
    TranscriptionFailed(String),
    InvalidInput(String),
    DownloadFailed(String),
    SidecarError(String),
    SidecarNotRunning,
    Timeout(u64),
    Io(#[from] std::io::Error),
    Json(#[from] serde_json::Error),
    Http(#[from] reqwest::Error),
}
```

---

## yapstack-transcription-sidecar

Binary. No public API. Communicates via JSON-line IPC.

**CLI**:
```
yapstack-transcription-sidecar
    [--engine whisper|parakeet]                # default: whisper (preserves prior CLI behavior)
    [--model /path/to/model[/dir]]              # ggml file for Whisper, model dir for Parakeet
    [--vad-model /path/to/silero.bin]           # Whisper only
    [--sortformer-model /path/to/sortformer.onnx]  # Parakeet only — enables per-call diarization
    [--coreml-cache-dir /path/to/cache/]        # Parakeet only — persistent CoreML compile cache
```

**Env vars**:
- `YAPSTACK_PARAKEET_ACCEL=auto|cpu|coreml|webgpu` — selects the ORT execution provider for Parakeet (`auto` = CoreML when no external `.onnx.data` files, else CPU)
- `RUST_LOG` — standard tracing override; default is `info,yapstack_transcription_sidecar=debug`

**Feature flags**:
- `whisper` — Whisper transcription via whisper-rs (requires cmake)
- `metal` — Metal acceleration for whisper-rs
- `parakeet` — Parakeet TDT via parakeet-rs + ORT (also enables Sortformer diarization)
- `coreml` — Adds the ORT-CoreML execution provider for Parakeet (Apple targets)
- `webgpu` — Adds the ORT-WebGPU execution provider for Parakeet (Metal under the hood on macOS)
- Default features = `["whisper", "parakeet"]`

If the engine the sidecar was spawned with isn't compiled in (e.g. `--engine parakeet` on a `--no-default-features --features whisper` build), every IPC request returns `engine 'X' not compiled in this build`.

**Internal architecture**: `engines/mod.rs` defines a `TranscriptionBackend` trait + shared text-cleanup helpers. Concrete impls in `engines/whisper.rs` (whisper-rs + flash attention + Silero VAD) and `engines/parakeet.rs` (ParakeetTDT + Sortformer post-pass + CoreML/WebGPU/CPU EP via env var). `main.rs` is a thin IPC dispatcher that picks one backend at startup and forwards every request through the trait.

**Whisper parameters**:
```rust
greedy(best_of=1)           // deterministic at temp 0 — all candidates identical
no_context(true)            // prevents cross-chunk context leakage
logprob_thold(-1.0)         // relaxed to avoid dropping valid speech
max_tokens(100/200)         // 100 for single-segment, 200 for multi-segment
suppress_blank(true)
suppress_nst(true)
temperature(0.0)
temperature_inc(0.2)        // fallback for noisy segments (retries at 0.2, 0.4…)
no_speech_thold(0.45)
entropy_thold(2.0)
single_segment(adaptive)    // true if audio <10s, false otherwise
```

**Silero VAD** (when `--vad-model` provided):
```rust
threshold: 0.5              // speech probability threshold
min_speech_duration: 250    // ms — minimum speech to consider
min_silence_duration: 100   // ms — minimum silence to split
speech_pad: 30              // ms — padding around speech segments
```
VAD runs as whisper.cpp preprocessing — non-speech audio is skipped before decoding, reducing hallucinations on silent/ambient segments.

**Audio preprocessing** (behind `whisper` feature):
- `to_mono(samples, channels)` — delegates to `yapstack_common::audio::deinterleave_to_mono()`
- `resample(samples, from_rate, to_rate)` — sinc interpolation resampling via rubato (Cubic, BlackmanHarris2, sinc_len 256, f_cutoff 0.98) to Whisper's expected 16kHz

**Hallucination filtering** (`should_include_segment(text, confidence) -> bool`):
```rust
// Filters out (in order):
// 1. Empty text or special tokens ([BLANK_AUDIO], [MUSIC], etc.)
// 2. Low confidence: confidence < 0.4
// 3. Excessive word repetition: 3+ consecutive identical words
//    - Direct check, then punctuation-normalized check via normalize_for_repetition()
//    - "Yeah.Yeah.Yeah." → "Yeah . Yeah . Yeah ." → detected as repetition
// 4. Known hallucination patterns at marginal confidence (< 0.6):
//    47 patterns including "thank you", "thanks for watching", "bye", "yeah",
//    "um", "so", "uh", "okay", "oh", "right", "hmm", "well", "you know",
//    "subscribe", "the end", etc.
// All filter hits logged at info! level for debugging.
```

---

## Tauri Commands (yapstack-desktop)

### Command Error Type
```rust
/// Unified error type for all Tauri commands. Serializes to { "kind": "...", "message": "..." }.
pub enum CommandError {
    Audio { message: String },
    Transcription { message: String },
    NotInitialized { message: String },
    InvalidInput { message: String },
    NotFound { message: String },
    Internal { message: String },
}
// From impls: AudioError, TranscriptionError, std::io::Error
```

### Managed State

Five managed states. The Tauri-facing names (rather than full type definitions) appear here; field shapes live with the Rust types and rotate freely without a doc change.

- `AudioManagerState` — audio capture lifecycle, ring buffers, capture-source tracking.
- `ModelManagerState` — Whisper / Parakeet / Sortformer model download + path resolution.
- `TranscriptionSchedulerState` — single source of truth for engine initialization. Wraps an `Option<InitializedEngine>` carrying the active `EngineConfig` and the long-lived `Arc<TranscriptionScheduler>`. The scheduler owns the sole `Arc<TranscriptionClient>` for the engine's lifetime; both runtimes clone it from here.
- `LiveTranscriptionState` — two named slots (`session`, `dictation`), each carrying explicit `Idle | Starting | Running | Stopping` lifecycle. See ARCHITECTURE.md § "Live Transcription" and the rustdoc on `RuntimeSlot` / `LiveTranscriptionSlots`.
- `DictationOwnsMicState` — mic-ownership window list (open + closed `[start, end)` intervals). See GLOSSARY.md "Mic-ownership window" and the rustdoc on `DictationOwnsMic` for the contract.

### Audio Commands (`commands/audio.rs`)
| Command | Args | Returns |
|---------|------|---------|
| `list_audio_devices` | — | `Vec<AudioDeviceInfoDto>` |
| `get_default_input_device` | — | `AudioDeviceInfoDto` |
| `start_capture` | `mic_device_id?, capture_source, capture_history_seconds?` | `()` |
| `stop_capture` | — | `()` |
| `get_capture_status` | — | `CaptureStatusDto` |
| `check_system_audio_permission` | — | `PermissionStatusDto` |
| `get_buffer_info` | — | `BufferStatusDto` |
| `peek_capture_energy` | `window_secs: f32` (must be finite > 0; `InvalidInput` otherwise) | `{ mic_rms, system_rms }` (live RMS over the last `window_secs`, used by the recording beacon) |

### Capture Commands (`commands/capture.rs`)
| Command | Args | Returns |
|---------|------|---------|
| `delete_session_wav` | `session_id, audio_save_location?` | `()` |
| `delete_audio_files` | `paths: Vec<String>` | `()` (or `Err(CommandError)` whose message lists every path that failed) |

`delete_session_wav` is the legacy session-glob cleanup path used by `clearAllSessions` and for sessions that pre-date the v15 `session_audio_parts` migration. Real per-part cleanup goes through `delete_audio_files`.

The session lifecycle (start, finalize, export) is owned by `start_live_transcription` / `stop_live_transcription` — there is no separate "instant capture" or `start_session` / `end_session` Tauri surface anymore. Audio finalization is driven by the live loop's streaming `SessionWavWriter` and the `session-part-ready` event; `session_audio_parts` is the durable source of truth.

`delete_audio_files` validates each path against the `TrustedAudioDirs` allow-list before unlinking. Failures are surfaced (not swallowed) so the FE can warn or queue retries; the error message lists every path that did not delete.

### Transcription Commands (`commands/transcription.rs`)
| Command | Args | Returns |
|---------|------|---------|
| `get_available_models` | — | `Vec<ModelInfoDto>` (Whisper) |
| `download_model` | `size` | `String` (path) |
| `delete_model` | `size` | `()` |
| `init_transcription_client` | `engine, whisper_model?, parakeet_variant?, enable_diarization` | `()` |
| `shutdown_transcription_client` | — | `()` |
| `get_transcription_status` | — | `TranscriptionStatusDto { initialized: bool }` |
| `get_engine_catalogue` | — | `Vec<EngineDescriptorDto>` (engine kinds × supported languages × capability flags) |
| `get_parakeet_models` | — | `Vec<ParakeetModelInfoDto>` |
| `download_parakeet_model` | `variant: ParakeetVariantDto` | `String` (dir path) |
| `delete_parakeet_model` | `variant: ParakeetVariantDto` | `()` |
| `get_sortformer_status` | — | `SortformerModelInfoDto` |
| `download_sortformer_model` | `variant: SortformerVariantDto` | `String` (path) |
| `delete_sortformer_model` | `variant: SortformerVariantDto` | `()` |

`init_transcription_client` validates that the requested engine's model is on disk (returns `NotFound` otherwise — frontend should call the appropriate `download_*` first), resolves the CoreML cache dir under `$APP_DATA_DIR/cache/coreml/`, optionally calls `ensure_sortformer` when `enable_diarization=true`, then spawns the sidecar with the engine + paths and constructs the long-lived `TranscriptionScheduler`.

Re-init semantics are precise:
- If the engine is uninitialized: build client + scheduler, OK.
- If already initialized with a *matching* `EngineConfig` (engine kind + Whisper model + Parakeet variant + diarization): return OK (idempotent — covers HMR remounts).
- If already initialized with a *different* config: return `InvalidInput` with "engine already initialized; call shutdown_transcription_client first". Engine swap is an explicit two-step operation; there is no implicit drain.

`shutdown_transcription_client` refuses with `InvalidInput` if any runtime slot is non-`Idle`; otherwise calls `scheduler.shutdown_client()` to drain the worker, take the client, and call `client.shutdown().await`.

All `download_*` commands emit `"model-download-progress"` events to the window with `{ percent: f32, ... }` (additional fields for Parakeet/Sortformer: `kind: "parakeet" | "sortformer"`, `variant`).

### Live Transcription Commands (`commands/live_transcription.rs`)
| Command | Args | Returns |
|---------|------|---------|
| `start_live_transcription` | `config: LiveTranscriptionConfig` | `LiveTranscriptionStartResult` |
| `stop_live_transcription` | `kind?: LiveSourceKind` | `()` |
| `get_live_transcription_status` | `kind?: LiveSourceKind` | `LiveTranscriptionStatus` |

`start_live_transcription` accepts a `source_kind: LiveSourceKind` field on `LiveTranscriptionConfig` (defaults to `Session` for legacy callers; the dictation hook passes `Dictation` explicitly). The command rejects if the target slot is non-`Idle` — both "already running" and "previous run still finalizing" cases. For `Dictation` runtimes it additionally enforces the boundary invariants `source == MicOnly`, `backfill_seconds == 0`, `persist_audio_part == false`; any deviation is an `InvalidInput`.

`stop_live_transcription` and `get_live_transcription_status` take an `Option<LiveSourceKind>`; `None` defaults to `Session`. Tauri/Specta cannot fill a missing invoke arg from a Rust default, so the parameter is required to be `Option<>` rather than defaulted at the type level — TS callers must pass `"session"` or `"dictation"` literally.

Emits `"live-transcription-segment"` events with `LiveSegmentEvent` (full shape: see `commands/live_transcription.rs`). Two routing fields, with strictly distinct meanings:
- `source_kind: "session" | "dictation"` — *routing identity*. Frontend listeners filter on this to decide which UI surface owns the event. The session segment store skips `dictation` events; the dictation hook accepts only `dictation` events with a matching synthetic `session_id`.
- `origin: "live" | "backfill" | "final_flush"` — *priority class* set by the scheduler at emit time. Tells the frontend which queue lane the chunk came from. **Never route on `origin`**: a dictation runtime's stop-time chunks are legitimately `final_flush`. See PRINCIPLES.md § "Routing identity is not priority class".

Other contract points on `LiveSegmentEvent`: `session_id` mirrors `LiveTranscriptionConfig.session_id` so multi-session listeners can route without stale-state guards. `LiveTranscriptionConfig.diarization` (optional, default false) is honored only when the active engine is Parakeet *and* the sidecar was spawned with a Sortformer model. Each `TranscriptSegmentDto` in `segments` carries `speaker_id: number | null`.

`stop_live_transcription` snapshots `AudioManager::buffer_positions()` plus a short tail grace into a `StopRequest` for the targeted slot. After that point, the targeted live loop skips normal VAD polling, idle cursor advancement, stream-health restarts, and unbounded WAV flushing. Final live chunks and the session WAV tail are read only up to that bounded stop position; later samples are ignored for the stopped runtime. The other slot's runtime (e.g. a session running while a dictation stops) is unaffected.

`LiveTranscriptionStatus` includes `lag_seconds`, `live_drain_backlog_chunks`, `live_drain_backlog_seconds`, and `source_kind`. The `source_kind` is load-bearing for the frontend's two-prong filter: status events without it would let a dictation `Stopped` flip the session UI's phase machine. The backlog fields signal that live audio is being preserved and drained because inference fell behind real time. `LiveTranscriptionPressureEvent` also carries `drain_backlog_seconds` per chunk.

Emits `"backfill-complete"` event (empty payload) when backfill processing finishes for a `Session` runtime. Suppressed for `Dictation` runtimes — dictation has `backfill_seconds == 0` so the event is meaningless, and emitting would clobber session backfill UI state when both runtimes are concurrent.

Emits `"session-part-ready"` event with `SessionPartReadyEvent { session_id, part_index, file_path, format: AudioExportFormatDto ("wav" \| "mp3"), duration_seconds, sample_rate }` when the streaming part is finalized after the loop exits. When `LiveTranscriptionConfig.persist_audio_part` is true (the default — actual sessions), a `session_audio_parts` row is inserted from Rust *before* this event fires, so the DB stays the durable source of truth even if the listener is gone. When the flag is false (dictation), the row insert is skipped — dictation owns its audio path on `dictation_history` instead, and the synthetic `session_id` has no `sessions.id` to FK against.

Emits `"session-wav-error"` event with `SessionWavErrorEvent { session_id, message }` when an empty recording is detected (0 samples written) or a finalize error occurs. The empty WAV file is deleted in this path.

Emits `"stream-health"` events with `StreamHealthEvent { source: AudioSourceLabel, status: String, message: String }` when a stream error or stall is detected and restart is attempted. Status values: `"restarted"`, `"restart_failed"`, `"restart_abandoned"`.

Emits `"live-transcription-warning"` event with `{ message }` when the sidecar is auto-restarted mid-session after a transcription failure (transient); the loop continues.

**`LiveTranscriptionConfig`** field-level shape is the source-of-truth on the Rust struct. Notable fields the frontend touches:
- `source_kind: LiveSourceKind` (default `Session`) — routing identity; the dictation hook passes `Dictation` explicitly. Drives every downstream filter.
- `silence_duration_ms` (default 800; Whisper-only — Parakeet uses a fixed 200 ms), `max_chunk_seconds` (default 30), `backfill_seconds` (default 0), `source`, `mix_config?`, `language?`, `prompt_context_chars?` (default 350), `prompt_decay_silence_seconds?` (default 5.0, set to 0 to disable).
- `session_id?` enables streaming session audio recording into a new part.
- `persist_audio_part?: bool` (default `true`; `false` for dictation so finalize skips the `session_audio_parts` insert that would FK-orphan against the synthetic id).
- `audio_save_location?`, `audio_export_format?: "wav" | "mp3"` (default `"mp3"`), `mp3_bitrate?` (kbps, validated against the LAME-supported set; default 64).
- `diarization?`, `vocabulary_hints?` (Whisper-only).
- `resume?: ResumeConfig` (prior cumulative duration → `session_offset_base_seconds`, next `part_index`).

For `source_kind: Dictation`, the boundary validator additionally requires `source == MicOnly`, `backfill_seconds == 0`, `persist_audio_part == false` (rejected with `InvalidInput` otherwise — failing fast beats letting an inconsistent dictation runtime emit segments into session storage paths).

**`LiveTranscriptionPhase`**: `Running`, `Stopped`, `Error`.

**Internal types** (not exported via Tauri): `TranscriptionContext`, `SessionWavState`, `SourceVadState`, `VadAction`, `ChunkResult`, `SegmentOrigin`, `RuntimeSlot`, `LiveTranscriptionSlots`, `MicOwnershipGuard`. Field-level shape lives with the rustdoc on each definition — see `commands/live_transcription.rs` for the source of truth. Two notes:
- `TranscriptionContext` holds `Arc<TranscriptionScheduler>` cloned from `TranscriptionSchedulerState` (replacing the prior per-session client + round-trip to `TranscriptionClientState`). Multiple runtimes hold cloned contexts that share the same scheduler.
- `SegmentOrigin` is the IPC mirror of `JobOrigin`. Maps `JobOrigin::Dictation → SegmentOrigin::Live` deliberately: a dictation runtime's mic chunks are tagged `JobOrigin::Dictation` for *queue priority*, but `origin` is the segment's priority class — frontends route by `source_kind`, never by `origin`. There is no `SegmentOrigin::Dictation` for that exact reason.

### Transcription Scheduler (`commands/transcription_scheduler.rs`)

Internal module — not exported via Tauri. See ARCHITECTURE.md § "Transcription Scheduler" for the design rationale, and the rustdoc on `TranscriptionScheduler` / `JobRequest` / `JobOutcome` / `SchedulerError` / `BusyKind` for current type shapes. The behavioural contract:

- **Long-lived singleton.** Constructed once at engine init by `init_transcription_client`, stored in `TranscriptionSchedulerState`, owns the sole `Arc<TranscriptionClient>` for the engine's full lifetime. Multiple runtimes (one session + one dictation) clone `Arc<Scheduler>` and submit into the same single worker.
- **Four priority tiers**: `FinalFlush > Dictation > Live (mic/system round-robin) > Backfill`. Dictation chunks jump the queue past session live chunks but cannot preempt an in-flight sidecar job — once a job has crossed into the sidecar, `Dictation` waits for it to finish.
- **Per-producer busy bitmask** `{LiveMic, LiveSystem, Dictation}` gates `Backfill`. Admitted only when every bit is clear; one runtime clearing its bit while another is still busy can't unblock backfill.
- **Sole caller of `TranscriptionClient::transcribe_with`**, which keeps sidecar respawn race-free (the worker is the only long-lived `Arc<TranscriptionClient>` holder, so `Arc::try_unwrap` succeeds at respawn time).
- **`submit()` rejects after shutdown.** Returns `Result<oneshot::Receiver<JobOutcome>, SchedulerError>` and rejects with `Shutdown` once the scheduler is non-`Running`. Protects `Arc<Scheduler>` clones held by racing callers (e.g. a session-stop racing engine shutdown) from enqueueing into a dead worker. Taking the scheduler out of app-state alone is insufficient — `Arc` clones outlive the slot.
- **Lifecycle states** `Running | ShuttingDown | Terminal`. `shutdown_client(timeout)` transitions through ShuttingDown to Terminal: drains the worker, takes the inner client, calls `client.shutdown().await`. Idempotent; safe to call multiple times. The 5-minute default timeout caps a wedged-sidecar worst case.

### Dictation Commands (`commands/dictation.rs`)
| Command | Args | Returns |
|---------|------|---------|
| `clipboard_paste` | `text: String, auto_paste: bool` | `()` |

Writes `text` to system clipboard via `pbcopy` (macOS) or `clip` (Windows). If `auto_paste` is true, waits 50ms then simulates Cmd+V via `osascript` (macOS only).

### Export Commands (`commands/export.rs`)
| Command | Args | Returns |
|---------|------|---------|
| `write_text_file` | `path: String, contents: String` | `()` |

Writes `contents` to `path` (the user's save-dialog choice) for transcript export. A trusted-renderer boundary: it refuses any path whose extension is not a text-export type (`md`/`markdown`/`txt`/`text`), and the `fs` plugin grant stays read-only so this is the only renderer-side write path.

### System Volume Commands (`commands/system_volume.rs`)
| Command | Args | Returns |
|---------|------|---------|
| `apply_volume_duck` | `target: f32` (0.0..=1.0) | `()` |
| `restore_volume` | — | `()` |

Generic ducking surface — the commands know nothing about dictation; callers decide policy. `apply_volume_duck` lowers the default output to `target`, snapshotting `(device_id, prior_level)` on the first apply of a duck cycle. No-op when the current level is already at or below `target` (no snapshot is taken — restore stays a no-op). `restore_volume` puts the snapshotted device back to its prior level even if the system default has since changed; it's also a no-op if no snapshot is held. Both commands swallow platform errors and return `()` because volume control is a UX nicety, not load-bearing for the caller. macOS only today; Windows / Linux are no-op stubs.

### Overlay Panel Commands (`commands/mod.rs`)
| Command | Args | Returns |
|---------|------|---------|
| `show_overlay_panel` | `label: String` | `()` |
| `hide_overlay_panel` | `label: String` | `()` |

Platform-specific: macOS uses `tauri-nspanel` `ManagerExt::get_webview_panel()` on the main thread, non-macOS falls back to `WebviewWindow` show/hide. Returns `Ok(())` even if window not found (no-op).

### Autostart Commands (`commands/mod.rs`)
| Command | Args | Returns |
|---------|------|---------|
| `get_autostart_enabled` | — | `bool` |
| `set_autostart_enabled` | `enabled: bool` | `()` |

Uses `tauri-plugin-autostart` `AutoLaunchManager` from app state. Errors mapped to `CommandError::Internal`.

### DTO Pattern
Domain types (yapstack-common, yapstack-audio, yapstack-transcription) do not derive `specta::Type`. Tauri commands use DTO structs that derive `specta::Type` + `Serialize` and implement `From<DomainType>`. This keeps the specta/tauri dependency out of library crates.

**`MixConfigDto`** validates gain values via `sanitized()` — non-finite or negative gains are clamped to `1.0`.

### Custom URI Scheme: `audio-stream`

Registered in `lib.rs` via `register_uri_scheme_protocol`. Serves session audio
files (`.wav` or `.mp3`) from any directory in the `TrustedAudioDirs` allow-list
— seeded at startup from `session_audio_parts` rows + `audio_save_locations`,
and grown each time Rust finalizes a part. Two URL forms:

```
audio-stream://localhost/{absolute-file-path}     # primary; emitted by convertFileSrc(absPath, "audio-stream")
audio-stream://localhost/{filename}               # legacy; resolves under $APP_DATA_DIR/audio/
```

| Method | Status | Description |
|--------|--------|-------------|
| GET (no Range header) | 200 | Full file, `Content-Type: audio/wav` or `audio/mpeg`, `Accept-Ranges: bytes` |
| GET (Range: bytes=N-M) | 206 | Partial content, `Content-Range: bytes N-M/total` |
| Bad extension / traversal | 400 | Path doesn't end in `.wav`/`.mp3`, or relative path contains `/`, `\`, `..` |
| Untrusted absolute path | 403 | Absolute path is outside every `TrustedAudioDirs` entry |
| File not found | 404 | File doesn't exist |

Frontend helper: `convertFileSrc(absPath, "audio-stream")` generates the URL.
Pass the full persisted part path (from `session_audio_parts.file_path`) — the
synthesized `{session_id}.{part_index}.{ext}` form would skip parts whose
`audio_save_location` differs from the current setting.

---

## SQLite Schema (Frontend)

Managed via `tauri-plugin-sql` with migrations in `src-tauri/src/db.rs`. Frontend types in `src/lib/db.ts`.

### Tables

**sessions**
| Column | Type | Notes |
|--------|------|-------|
| `id` | TEXT PK | UUID |
| `title` | TEXT | Default '' |
| `created_at` | TEXT | `datetime('now')` |
| `updated_at` | TEXT | `datetime('now')` |
| `source` | TEXT | 'Mixed', 'MicOnly', 'SystemOnly' |
| `status` | TEXT | 'recording', 'completed' |
| `duration_seconds` | REAL | Nullable |
| `total_segments` | INTEGER | Default 0 |
| `folder_id` | TEXT FK | Nullable, references folders(id) |
| `is_pinned` | INTEGER | 0 or 1 |
| `pinned_at` | TEXT | Nullable |
| `session_type` | TEXT | 'transcription' or 'manual' |
| `wav_file_path` | TEXT | Nullable. Legacy column retained as a fallback duration source; `session_audio_parts.file_path` is the durable source of truth (and may point to `.wav` or `.mp3`). |
| `wav_duration_seconds` | REAL | Nullable. Legacy fallback when no `session_audio_parts` rows exist. |
| `sort_order` | INTEGER | Default 0 |

**segments**
| Column | Type | Notes |
|--------|------|-------|
| `id` | TEXT PK | UUID |
| `session_id` | TEXT FK | References sessions(id) CASCADE |
| `source` | TEXT | 'Mic', 'System' |
| `text` | TEXT | Current text (may be edited) |
| `audio_offset_seconds` | REAL | Timestamp in session audio |
| `chunk_duration_seconds` | REAL | |
| `confidence` | REAL | Default 1.0 |
| `created_at` | TEXT | |
| `chunk_index` | INTEGER | |
| `original_text` | TEXT | Nullable, set on first edit |
| `edited_at` | TEXT | Nullable |
| `deleted_at` | TEXT | Nullable (soft delete) |
| `hidden` | INTEGER | 0 or 1 |
| `speaker_id` | INTEGER | Nullable. Set by Parakeet+Sortformer diarization. *Not* added via the migration list — added by the frontend's `getDb()` after `tauri-plugin-sql` migrations run (idempotent ALTER) to sidestep a ghost v11 entry on dev DBs. |

**folders**
| Column | Type | Notes |
|--------|------|-------|
| `id` | TEXT PK | UUID |
| `name` | TEXT | |
| `parent_id` | TEXT FK | Nullable, self-referencing |
| `sort_order` | INTEGER | Default 0 |
| `icon` | TEXT | Nullable, emoji or icon identifier |
| `color` | TEXT | Nullable, hex color |
| `description` | TEXT | Nullable, folder description for AI context |
| `created_at` | TEXT | |
| `updated_at` | TEXT | |

**notes**
| Column | Type | Notes |
|--------|------|-------|
| `id` | TEXT PK | UUID |
| `session_id` | TEXT FK UNIQUE | References sessions(id) CASCADE |
| `content` | TEXT | HTML from Tiptap editor |
| `updated_at` | TEXT | |

**note_versions**
| Column | Type | Notes |
|--------|------|-------|
| `id` | TEXT PK | UUID |
| `note_id` | TEXT FK | References notes(id) CASCADE |
| `content` | TEXT | Snapshot HTML |
| `created_at` | TEXT | |

**shares**
| Column | Type | Notes |
|--------|------|-------|
| `id` | TEXT PK | UUID |
| `folder_id` | TEXT FK | References folders(id) CASCADE |
| `shared_with_email` | TEXT | Nullable |
| `permission` | TEXT | 'viewer' default |
| `created_at` | TEXT | |
| `expires_at` | TEXT | Nullable |

**chat_messages**
| Column | Type | Notes |
|--------|------|-------|
| `id` | TEXT PK | UUID |
| `context_key` | TEXT NOT NULL | Chat context identity (session ID, folder ID, "all", or "pinned"). Index: `idx_chat_messages_context(context_key, created_at)`. |
| `session_id` | TEXT FK | Nullable. References sessions(id) CASCADE. Null for multi-session contexts. |
| `role` | TEXT | "user" or "assistant" |
| `content` | TEXT | Message text. Assistant messages may have `[tool:name] detail` prefix lines for tool badges. |
| `action` | TEXT | Nullable. AIActionType that triggered this message (e.g., "summarize", "key-points") |
| `created_at` | TEXT | ISO timestamp |

**session_folders** (many-to-many junction table)
| Column | Type | Notes |
|--------|------|-------|
| `session_id` | TEXT FK | References sessions(id) CASCADE |
| `folder_id` | TEXT FK | References folders(id) CASCADE |
| | UNIQUE | `(session_id, folder_id)` |

Indexes: `idx_session_folders_session(session_id)`, `idx_session_folders_folder(folder_id)`.

**dictation_history**
| Column | Type | Notes |
|--------|------|-------|
| `id` | TEXT PK | UUID |
| `slot_id` | TEXT | Dictation slot ID |
| `slot_name` | TEXT | Slot name at time of dictation |
| `input_text` | TEXT | Raw transcription text |
| `output_text` | TEXT | Final output (after AI processing if enabled) |
| `ai_enabled` | INTEGER | 0 or 1 |
| `ai_prompt` | TEXT | AI prompt used (if ai_enabled) |
| `output_action` | TEXT | 'paste', 'clipboard', or 'new-note' |
| `wav_file_path` | TEXT | Nullable, absolute path to the captured audio file (WAV or MP3 — column name is legacy). |
| `wav_duration_seconds` | REAL | Nullable, captured audio duration in seconds. |
| `session_id` | TEXT | Nullable, correlated session ID (for new-note output) |
| `created_at` | TEXT | `datetime('now')` |

Indexes: `idx_dictation_history_created(created_at)`, `idx_dictation_history_slot(slot_id)`.

### Migration History
| Version | Description |
|---------|-------------|
| 1 | sessions + segments tables |
| 2 | folders table, sessions.folder_id, is_pinned, pinned_at |
| 3 | Segment editing: original_text, edited_at, deleted_at, hidden |
| 4 | notes + note_versions tables, sessions.session_type |
| 5 | sessions.wav_file_path, wav_duration_seconds (superseded by v15; columns retained as fallback duration source) |
| 6 | sessions.sort_order, shares table (table is currently dormant — defined but unused in app code) |
| 7 | chat_messages table (CASCADE on session delete) |
| 8 | chat_messages: add `context_key` NOT NULL, make `session_id` nullable, add `idx_chat_messages_context` index |
| 9 | folders: add `icon`, `color`, `description` columns. New `session_folders` junction table (many-to-many) with unique constraint and indexes |
| 10 | `dictation_history` table with indexes |
| 11 | `tags` and `session_tags` tables (flat, AI-applied metadata; folders remain the primary organizational primitive) |
| 12 | FTS5 search tables (`segments_fts`, `notes_fts`, `sessions_fts`, `dictations_fts`) with backfill + sync triggers |
| 13 | `chat_messages.tool_calls` column (persisted tool-call stream for replay) |
| 14 | `chat_messages` per-LLM-response columns (`send_id`, `sequence`, `tool_call_id`, `observation`, `status`) for accurate multi-turn replay |
| 15 | `session_audio_parts` table (id, session_id, part_index, file_path, format, duration_seconds, sample_rate, created_at). Backfilled from legacy `wav_file_path` rows. Durable source of truth for session audio files; resumable sessions append a new row per resume. |

**Runtime schema patches** (in `db::ensure_runtime_schema()`, applied before `tauri-plugin-sql` initializes):

- Sweeps stale `recording`-status sessions left by a prior crash (recomputes duration from `session_audio_parts` or `segments`, marks them `completed`).
- Creates `audio_save_locations` table (idempotent) — every directory the app has written audio into. Used by `scan_missing_audio_parts()` on next startup to recover orphan part files if a row insert was missed.

Additional schema is created by the **frontend's** `getDb()` (`ensureRuntimeSchema`) after migrations run, deliberately kept out of the versioned Rust list to sidestep a "ghost" v11 entry that some local dev DBs picked up from another branch (which makes sqlx silently refuse later numbered migrations). See [ADR 0002](adr/0002-chat-context-settings-frontend-schema.md):

- `segments.speaker_id INTEGER` — Parakeet+Sortformer diarization.
- `chat_context_settings` table (`context_key` PK, `profile_id` nullable, `updated_at`) — per-chat-context AI **Profile** override; `NULL`/absent means "use the live default Chat assignment". Created idempotently with `CREATE TABLE IF NOT EXISTS`.

### Settings Persistence (Zustand)

Settings are stored via Zustand's `persist` middleware with `localStorage`. Schema versioned (currently **v31**).

| Version | Description |
|---------|-------------|
| 0→1 | `graceSeconds` → `backfillSeconds` |
| 1→2 | Added `silenceDurationMs`, `maxChunkSeconds`, `overlapSeconds` |
| 2→3 | Reset aggressive defaults (500ms→800ms, 15s→30s, 0.5s→1.0s) |
| 3→4 | Added `promptContextChars` |
| 4→5 | Added `theme` (light/dark/system) |
| 5→6 | Added `sidebarCollapsed` |
| 6→7 | Added `bufferMaxSeconds` (300), removed `backfillSeconds` |
| 7→8 | Added `ai` settings (provider config, API keys, model selection) |
| 8→9 | Added `shortcutBindings` (Record<string, string> override map) |
| 9→10 | Added `audioSaveLocation: string \| null` |
| 10→11 | Added `dictation: DictationSettings` with defaults |
| 11→12 | Added `outputAction` field to existing `DictationSlot`s (default `"paste"`) |
| 12→13 | Added `showRecordingIndicator: boolean` (default `true`) |
| 13→14 | Changed default model `Base` → `Small`, default capture `MicOnly` → `Mixed` (migrates existing users) |
| 14→15 | Added `promptDecaySilenceSeconds` (default 5) — seconds of all-source silence before clearing prompt context |
| 15→16 | Added `activationMode` to dictation settings (default `"hold"`) |
| 16→17 | Existing users marked `onboardingCompleted = true` so they skip the first-run flow |
| 17→18 | `onboardingCompleted` boolean → structured `onboarding: { completedFlows: Record<string, ISO> }` |
| 18→19 | Default `Control+Shift+Space` binding written to dictation slot 1 if missing |
| 19→20 | Replace name-based `selectedMicDevice` with stable id-based `selectedMicDeviceId` (reset to null on upgrade — user re-picks once) |
| 20→21 | Added `audioExportFormat` (default `"mp3"`) and `mp3Bitrate` (default 64) |
| 21→22 | Engines become first-class peers: added `selectedEngine` (default `"Whisper"`), `selectedParakeetVariant` (`"TdtV3"`), `diarizationEnabled` (`false`), `speakerNames: Record<string, Record<number, string>>` |
| 22→23 | Force-disable `diarizationEnabled` on upgrade. Sortformer's chunk-local speaker IDs cause the same person to flip across speaker numbers across chunk boundaries; the IPC + DB + sidecar plumbing stays intact so re-enable is one line away once session-stable IDs land. |
| 23→24 | Added dictation volume-ducking settings (`dictationDuckEnabled`, `dictationDuckTarget`; later reduce-by `dictationDuckAmount`). |
| 24→25 | AI refactor: populate `aiConfig` (Connections / Profiles / Assignments) from the legacy single-provider `ai` shape; set each dictation slot's `profileId`. Legacy `ai` left in place as the re-migration safety net. |
| 25→26 | Added `insights: InsightsSettings` (Live Insights) with empty defaults. |
| 26→27 | Insights: `activeInsightId` → `defaultInsightId` (session-start default; the runtime "current" insight is non-persisted). |
| 27→28 | Insight cadence: per-slot `heartbeatSeconds` (fixed interval) → `trigger` (threshold / settle / floor / ceiling), mapped to a behavior-preserving interval. |
| 28→29 | Insight cadence presets renamed (`summary`/`jargon`/`topic` → tempo tiers `responsive`/`balanced`/`relaxed`); added Insight `type`. |
| 29→30 | Dropped the redundant per-Insight `enabled` flag. |
| 30→31 | Remove the redundant `_legacyAi` snapshot written by an early build of the AI refactor (duplicate of legacy `ai`, including plaintext keys, never read). |

---

## AI Chat Frontend API

### `lib/ai.ts`

> **Connection/Profile model** (replaces the legacy single-active-provider `AISettings`). A **Connection** is a credential+endpoint; a **Profile** binds a Connection to a model; **Assignments** map features → Profiles. The legacy `ai` shape is migrated into `aiConfig` at persist v25 (see Settings Persistence) and is no longer read by the app.

**Types**
| Type | Fields |
|------|--------|
| `AIProviderKind` | `"openai" \| "openrouter" \| "custom"` |
| `Connection` | `{ id, name, kind: AIProviderKind, baseUrl, apiKey, availableModels?, fetchedAt?, fetchError? }` |
| `Profile` | `{ id, name, connectionId, model }` |
| `AIAssignments` | `{ chatProfileId: string \| null, aiActionsProfileId: string \| null }` |
| `AIConfig` | `{ connections: Connection[], profiles: Profile[], assignments: AIAssignments }` |
| `ChatMessage` | `{ id, role, content, action?: AIActionType, isStreaming?, toolExecutions? }` |
| `FileAttachment` | `{ name, content }` |
| `AIActionType` | `string` — any action ID. Built-in values: `"summarize"`, `"key-points"`, `"action-items"`, `"meeting-minutes"`, `"general"`. Extensible for custom actions. |
| `ChatContext` | `{ type: "session", sessionId } \| { type: "folder", folderId } \| { type: "all" } \| { type: "pinned" } \| { type: "dictation" }` |
| `StreamEvent` | `{ type: "token", content } \| { type: "tool_calls", calls: ToolCallResult[] } \| { type: "done" }` |

**Functions**
| Function | Signature | Description |
|----------|-----------|-------------|
| `createAIClientForConnection` | `(connection: Connection) → OpenAI` | Creates an OpenAI client with kind-specific headers (blank key allowed for `custom`) |
| `resolveProfile` | `(config: AIConfig, profileId: string \| null) → { profile, connection }` | Resolves a Profile id to its Profile + Connection (throws if missing/dangling) |
| `resolveAndCreateClient` | `(config: AIConfig, profileId: string \| null) → { client: OpenAI, model: string }` | Resolves a Profile and builds its client + model in one call |
| `fetchCustomModels` | `(baseUrl: string, apiKey?: string) → Promise<string[]>` | Fetches the `/models` catalog for a Connection |
| `shouldAutoRefreshModels` | `(args) → boolean` | Whether the editor should re-fetch the catalog on save (endpoint or key changed) |
| `chatContextKey` | `(ctx: ChatContext) → string` | Returns context identity string for chat message persistence |
| `assembleTranscriptContext` | `(segments: DbSegment[]) → string` | Formats segments as `[seg:ID timestamp] text` lines. Filters hidden/deleted. |
| `assembleNoteContext` | `(noteHtml: string) → string` | Strips HTML to plain text |
| `assembleMultiSessionContext` | `(sessions: SessionWithNote[], includeNotes: boolean) → string` | Formats multiple sessions with optional notes for multi-session prompts |
| `buildMessages` | `(action, transcript, notes, attachments, history, userMessage?, sessionMeta?) → ChatCompletionMessageParam[]` | Builds system + history + user messages. Resolves `action` ID to directive via `ACTIONS` lookup, falling back to `GENERAL_DIRECTIVE`. Uses `getSystemPromptWithToolContext` when `sessionMeta` provided. |
| `buildMultiSessionMessages` | `(context, attachments, history, userMessage) → ChatCompletionMessageParam[]` | Builds messages for multi-session chat contexts |
| `streamChat` | `async* (client, model, messages, signal?) → AsyncGenerator<string>` | Simple text streaming (no tools) |
| `streamChatWithTools` | `async* (client, model, messages, tools, signal?) → AsyncGenerator<StreamEvent>` | Streaming with tool call accumulation |
| `markdownToBasicHtml` | `(md: string) → string` | Converts markdown to HTML via `marked` |
| `testConnection` | `(connection: Connection, model: string) → Promise<{ ok, error? }>` | Issues a minimal chat completion to confirm the Connection reaches a working server with valid credentials |
| `assembleDictationContext` | `(entries: DbDictationHistory[]) → string` | Formats dictation history entries for AI context |

### `lib/ai-actions.ts`

Self-contained action definitions. Each action carries its own system prompt directive text — no external lookup table.

**Types**
| Type | Fields |
|------|--------|
| `ActionDefinition` | `{ id: string, label: string, description: string, icon: LucideIcon, requiresTranscript?: boolean, directive: string }` |

- `requiresTranscript` — when `true`, the action is hidden for manual (non-transcription) sessions
- `directive` — the full system prompt text for this action, including tool-calling instructions

**Constants & Functions**
| Export | Description |
|--------|-------------|
| `ACTIONS` | Array of 4 `ActionDefinition`s: `summarize`, `key-points`, `action-items`, `meeting-minutes`. Each carries its own `directive`. |
| `getAction(id)` | Look up action by ID. Returns `ActionDefinition \| undefined`. |
| `getActionIcon(id)` | Get icon component for action ID. Returns `LucideIcon \| undefined`. |
| `getActionsForSession(sessionType)` | Returns actions filtered by session type. Manual sessions exclude actions with `requiresTranscript: true`. |

**Extensibility**: A custom action is just an `ActionDefinition` object with an `id`, `label`, `icon`, and `directive`. No core files need modification — pass custom actions alongside built-ins through the `AIContextProvider`.

### `lib/ai-tools.ts`

Modular tool registry. Each tool is self-contained with schema, executor, undo handler, and effect metadata.

**Types**
| Type | Fields |
|------|--------|
| `ToolCallResult` | `{ id, name, arguments: Record<string, unknown> }` |
| `ExecutedTool` | `{ name, label, detail, observation?, toolCallId?, result?, undoData? }` — `observation` is the text the LLM sees as the tool result during multi-turn chains; `detail` is the human-facing badge |
| `ToolContext` | Discriminated union: `SessionToolContext { scope: "session"; sessionId; currentTitle; currentNote: DbNote \| null; isPinned; segments?; tags?; folderNames?; folderIds?; allowedSessionIds? }` for single-session chats, or `RetrievalToolContext { scope: "retrieval"; allowedSessionIds }` for folder/pinned/all chats. Mutating tools narrow via `requireSessionContext(ctx)`. |
| `requireSessionContext` | `(ctx: ToolContext) → SessionToolContext` | Narrows the union to the session-scoped variant. Throws if called from a retrieval-only chat — that combination is a wiring bug. |
| `ToolKind` | `"read" \| "mutate"` — gates Undo eligibility and the "Session updated" toast |
| `ToolEffect` | `"session-meta" \| "notes" \| "organization" \| "transcript"` — declarative side-effect categories |
| `ToolDefinition` | `{ kind: ToolKind, schema: ChatCompletionTool, execute: (args, ctx) → Promise<ExecutedTool \| null>, undo: (undoData, ctx) → Promise<void>, affects?: ToolEffect[] }` |

**Functions**
| Function | Signature | Description |
|----------|-----------|-------------|
| `registerTool` | `(def: ToolDefinition) → void` | Registers a tool in the global registry |
| `getRegisteredTools` | `() → ChatCompletionTool[]` | Returns every registered tool's schema |
| `getToolsById` | `(toolIds: string[]) → ChatCompletionTool[]` | Returns schemas for specific tool IDs. Per-context tool selection is the caller's job — see `createSessionTools` / `createMultiSessionTools` in `ai-context.ts`. |
| `getToolKind` | `(toolName: string) → ToolKind \| undefined` | Returns the registered tool's `"read"` / `"mutate"` discriminator (used by the chat UI to render the right badge and gate undo eligibility). |
| `getToolEffects` | `(toolNames: string[]) → Set<ToolEffect>` | Collects effect categories from executed tool names. Used by `NoteDetailView` to determine which data to refresh after tool execution. |
| `executeTool` | `(name, args, ctx) → Promise<ExecutedTool \| null>` | Executes a tool by name. Returns `null` if the tool is a no-op (e.g., title unchanged). |
| `captureUndoSnapshot` | `<T>(loader: () => Promise<T>) → Promise<T>` | Helper that loads pre-mutation state for the executor's `undoData`, used by mutating tools so undo restores the prior value. |
| `undoToolCalls` | `(executed, ctx) → Promise<void>` | Reverses tool executions in LIFO order |
| `convertCitationsToSegmentRefs` | `(html, segments) → string` | Replaces `[[seg:ID]]` text citations with `<span data-segment-ref>` HTML nodes for Tiptap rendering |

**Registered Tools**
| Tool Name | Parameters | DB Operations | Effects |
|-----------|-----------|---------------|---------|
| `update_title` | `{ title: string }` | `updateSessionTitle()` | `["session-meta"]` |
| `save_to_notes` | `{ content: string, mode: "replace" \| "append" \| "prepend" \| "find_replace", find?: string }` | `markdownToBasicHtml()` → `convertCitationsToSegmentRefs()` → `saveNote()` (per mode: overwrite / append below / prepend above / surgical substring swap) | `["notes"]` |
| `pin_session` | `{ pinned: boolean }` | `togglePin()` (conditional) | `["session-meta"]` |
| `tag_session` | `{ add: string[], remove?: string[] }` | `getTagByName()` → `createTag()` → `addSessionTag()` / `removeSessionTag()` | `["organization"]` |
| `search_folders` | `{ query?: string }` | `listFolders()` → optional substring filter against folder names + descriptions | `[]` (read-only) |
| `add_session_to_folder` | `{ folder_id: string }` | `findBranchConflicts()` → `dbAddSessionToFolder()` → `getFolderPath()` (returns hierarchical description chain in `result`) | `["organization"]` |
| `search_sessions` | `{ query: string, filters: { folder_id: string \| null, pinned: boolean \| null } \| null, limit?: number }` | FTS5 search across session titles, notes, and segment text. `filters` may be `null` for an unfiltered search; pass an object to restrict by folder and/or pin state. | `[]` (read-only) |
| `search_dictations` | `{ query: string, limit?: number }` | FTS5 search across `dictation_history` | `[]` (read-only) |
| `get_session_context` | `{ session_ids: string[], scope: "segments" \| "notes" \| "summary" \| "all" }` | For each id (max 5 per call): `getSession()` + scope-conditional `getSessionSegments()` / `getNote()`. `scope="summary"` is currently always null pending a future summarization step. Errors out-of-scope ids when the chat context carries `allowedSessionIds`. | `[]` (read-only) |
| `replace_in_transcript` | `{ find: string, replace: string, case_sensitive: boolean }` | Iterate `ctx.segments`, build per-segment `EditPlan` (skip deleted/hidden, escape regex, default case-insensitive). Capped at 50 affected segments per call — narrow `find` if you exceed it. Persists via `updateSegmentText()`, preserving each segment's `original_text` snapshot. | `["transcript"]` (refresh transcript views) |

### `lib/ai-prompts.ts`

Prompt assembly. All functions take a `directive: string` (not an action ID) — the caller resolves action to directive before calling.

**Types**
| Type | Fields |
|------|--------|
| `SessionMeta` | `{ title: string, isPinned: boolean, hasNotes: boolean }` |
| `FolderContextLayer` | `{ name: string, description: string }` — hierarchical folder context for multi-session prompts |

**Constants**
| Export | Description |
|--------|-------------|
| `GENERAL_DIRECTIVE` | Default system prompt for freeform chat (no specific action). Describes available tools and when to use them. Used as fallback when no action is selected. |

**Functions**
| Function | Signature | Description |
|----------|-----------|-------------|
| `getSystemPrompt` | `(directive: string, transcriptText, noteText, attachments) → string` | Builds system prompt: directive + citation instruction (if transcript present) + notes guidance + context sections |
| `getSystemPromptWithToolContext` | `(directive: string, transcriptText, noteText, attachments, sessionMeta: SessionMeta) → string` | Extends `getSystemPrompt` with session metadata footer (title, pin state, has notes) |
| `getMultiSessionSystemPrompt` | `(sessionsContext, attachments, folderContext?: FolderContextLayer[]) → string` | Builds multi-session prompt with optional folder context layers |
| `getDictationSystemPrompt` | `(dictationContext, attachments) → string` | Builds system prompt for dictation history AI chat |

### `lib/ai-context.ts`

Context orchestration layer. Provides factory functions to create context sources, tools, and prompt builders for different chat contexts (single session, multi-session, folder, etc.).

**Types**
| Type | Fields |
|------|--------|
| `ContextSource` | `{ id, type: "transcript" \| "notes" \| "sessions" \| "dictation", label, icon: LucideIcon, enabled, toggleable, summary?, assembler: () => Promise<string> }` |
| `AIContextTools` | `{ availableToolIds: string[], getToolContext: (() => Promise<ToolContext>) \| null, contextType?: "session" \| "multi-session" }` |
| `SystemPromptBuilder` | `(directive: string, contextParts: Record<string, string>, attachments: FileAttachment[]) => Promise<string>` — first param is the action's directive text, not an action ID |
| `AIContextValue` | `{ contextKey, sources: ContextSource[], toggleSource, tools: AIContextTools, actions: ActionDefinition[], segments: DbSegment[], buildSystemPrompt: SystemPromptBuilder, isSessionContext, sessionId: string \| null, onToolsExecuted, placeholder: string }` |
| `ListChatContext` | `{ contextKey: string, sources: ContextSource[], tools: AIContextTools, buildSystemPrompt: SystemPromptBuilder, placeholder: string }` |
| `ListContextConfig` | `{ filter: ListFilter, sessions: DbSession[], folders: Folder[] }` |

**Factory Functions**
| Function | Signature | Description |
|----------|-----------|-------------|
| `createSessionSources` | `(sessionId, segmentCount, sessionType) → ContextSource[]` | Transcript + notes sources. Omits transcript source for `"manual"` session type. |
| `createSessionTools` | `(sessionId) → AIContextTools` | All ten tools (`update_title`, `save_to_notes`, `pin_session`, `tag_session`, `search_folders`, `add_session_to_folder`, `search_sessions`, `search_dictations`, `get_session_context`, `replace_in_transcript`) with session-specific `ToolContext` (fetches live session/note/segment state). `contextType: "session"`. |
| `createSessionSystemPromptBuilder` | `(sessionId) → SystemPromptBuilder` | Returns builder that fetches session metadata and calls `getSystemPromptWithToolContext(directive, ...)`. |
| `createMultiSessionSources` | `(sessionIds, count) → ContextSource[]` | Sessions list source (non-toggleable) + notes source (toggleable). |
| `createMultiSessionTools` | `(allowedSessionIds: string[]) → AIContextTools` | Retrieval-only tools (`search_sessions`, `get_session_context`, `search_folders`, `search_dictations`); no mutations because they need a single `sessionId` and applying them at folder scope is ambiguous. `allowedSessionIds` is the load-bearing field — it pins retrieval to the chat's filter (folder / pinned / all) so the model can't reach outside the user's view. `contextType: "multi-session"`. |
| `createMultiSessionSystemPromptBuilder` | `(folderContext?: FolderContextLayer[]) → SystemPromptBuilder` | Returns builder that calls `getMultiSessionSystemPrompt` with optional folder context layers. Ignores directive param (multi-session uses its own fixed prompt). |
| `createDictationSources` | `(entryCount: number) → ContextSource[]` | Dictation history source (non-toggleable). |
| `createDictationTools` | `() → AIContextTools` | Retrieval surface scoped to dictation only — exposes `search_dictations` and nothing else. `allowedSessionIds: []` so any future tool that consults the field fails closed. `contextType: "multi-session"`. |
| `createDictationSystemPromptBuilder` | `() → SystemPromptBuilder` | Returns builder that calls `getDictationSystemPrompt`. |
| `resolveListContext` | `(filter: ListFilter, sessions: DbSession[], folders: Folder[]) → ListChatContext` | Centralizes context resolution for all `ListFilter` types (all, pinned, folder, dictation). Returns contextKey, sources, tools, systemPromptBuilder, and placeholder. |
| `chatContextKey` | Re-exported from `ai.ts` | `(ctx: ChatContext) → string` — context identity for chat messages |

### `components/AIContextProvider.tsx`

**Props**: `contextKey`, `sources: ContextSource[]`, `tools: AIContextTools`, `actions: ActionDefinition[]`, `segments: DbSegment[]`, `buildSystemPrompt: SystemPromptBuilder`, `isSessionContext`, `sessionId: string | null`, `onToolsExecuted`, `placeholder?: string`, `children`

**Hook**: `useAIContext() → AIContextValue | null` — accesses the nearest `AIContextProvider`'s value via React context.

The provider manages source toggle state (users can enable/disable transcript and notes independently) and passes the assembled `AIContextValue` to `FloatingChatBar` and other consumers.

### `components/FloatingChatBar.tsx`

The main AI chat interface. Positioned as a floating overlay inside the notes pane via Radix Collapsible. Delegates chat logic to `useChatMessages` hook and input UI to `ChatInputBar`.

### `hooks/useChatMessages.ts`

```typescript
function useChatMessages(
  ctx: AIContextValue | null,
  input: string,
  setInput: (value: string) => void,
  attachments: FileAttachment[],
  setIsExpanded: (value: boolean) => void,
): UseChatMessagesReturn
```

Returns `{ messages, isStreaming, undoState, handleSend, handleUndo, handleClearChat }`. Manages the full chat lifecycle: loading messages from DB, streaming with tool calls, executing tools with undo (10s timeout), and DB persistence. Auto-aborts on unmount.

`handleSend(actionDef?: ActionDefinition)` resolves `directive` from `actionDef.directive` or falls back to `GENERAL_DIRECTIVE`. Passes directive to `buildSystemPrompt(directive, contextParts, attachments)`.

### `components/chat/ChatInputBar.tsx`

Input component extracted from `FloatingChatBar`. Handles textarea auto-resize, file attachment (500KB limit), actions popover, context/model pills, and keyboard shortcuts (Enter to send, Shift+Enter for newline).

### `components/chat/ContextPill.tsx`

Toggleable pill showing context source status (transcript, notes). Static display if `onToggle` not provided; toggleable button with conditional styling otherwise.

### `components/chat/ModelPickerPill.tsx`

Popover-based model selector. Groups models by provider, shows recommended models, integrates with app store AI settings.

### `lib/db.ts` — Types & Chat Message Functions

**Type aliases**
| Type | Definition |
|------|-----------|
| `SessionStatus` | `"recording" \| "completed"` |
| `SessionType` | `"manual" \| "transcription"` (matches the `sessions.session_type` DB column) |

These replace raw strings in `DbSession.status` and `DbSession.session_type` respectively.

**Chat message functions**
| Function | Signature | Description |
|----------|-----------|-------------|
| `insertChatMessage` | `(msg: DbChatMessage) → Promise<void>` | Insert new message. `DbChatMessage` has `context_key` and nullable `session_id`. |
| `getChatMessages` | `(contextKey: string) → Promise<DbChatMessage[]>` | Get all messages for a context key, ordered by created_at ASC |
| `updateChatMessageContent` | `(id, content) → Promise<void>` | Update message content (used for streaming finalization and undo) |
| `deleteChatMessages` | `(contextKey: string) → Promise<void>` | Delete all messages for a context key |
| `getNotesForSessions` | `(sessionIds: string[]) → Promise<SessionWithNote[]>` | Get sessions with their notes for multi-session context assembly |

**Dictation history functions**
| Function | Signature | Description |
|----------|-----------|-------------|
| `insertDictationHistory` | `(entry: DbDictationHistory) → Promise<void>` | Insert new dictation history entry |
| `listDictationHistory` | `(limit?: number) → Promise<DbDictationHistory[]>` | List entries ordered by created_at DESC |
| `getDictationHistory` | `(id: string) → Promise<DbDictationHistory \| null>` | Get single entry by ID |
| `deleteDictationHistory` | `(id: string) → Promise<void>` | Delete single entry |
| `clearDictationHistory` | `() → Promise<void>` | Delete all entries |
| `updateDictationHistorySessionId` | `(id: string, sessionId: string) → Promise<void>` | Correlate entry with a session (for WAV linkage) |

---

## Keyboard Shortcuts Frontend API

### `lib/shortcuts.ts`

**Types**
| Type | Definition |
|------|-----------|
| `ShortcutCategory` | `"Recording" \| "Navigation" \| "Editor" \| "General" \| "Dictation"` |
| `ShortcutDefinition` | `{ id: string, label: string, description: string, category: ShortcutCategory, defaultBinding: string, isGlobal?: boolean, isDictation?: boolean }` |

**Constants**
| Export | Description |
|--------|-------------|
| `SHORTCUT_CATEGORIES` | Array of 5 `ShortcutCategory` values |
| `SHORTCUTS` | Array of 17 `ShortcutDefinition` objects (4 global, 13 in-app) |
| `SHORTCUT_MAP` | `Map<string, ShortcutDefinition>` for fast lookup by ID |
| `shortcutCaptureActive` | `{ current: boolean }` — shared ref to suppress shortcuts during rebinding |
| `CODE_TO_KEY` | `Record<string, string>` — maps `e.code` to display key |
| `MODIFIER_CODES` | `Set<string>` — filter out modifier-only presses |

**Functions**
| Function | Signature | Description |
|----------|-----------|-------------|
| `getBinding` | `(id: string, overrides: Record<string, string>) → string` | Returns user override or default binding for a shortcut ID |
| `eventToBinding` | `(e: KeyboardEvent) → string \| null` | Converts KeyboardEvent to `"mod+shift+k"` format (in-app shortcuts) |
| `eventToGlobalBinding` | `(e: KeyboardEvent) → string \| null` | Converts KeyboardEvent to `"CommandOrControl+Shift+N"` format (Tauri global shortcuts) |

### `hooks/useKeyboardShortcuts.ts`

```typescript
export function useKeyboardShortcuts(): void
```

In-app shortcuts via capture-phase `keydown` listener. Mounted in `AppLayout`. Skips when `shortcutCaptureActive.current` is true or when input/textarea/contenteditable is focused. 12 in-app actions: command-palette, toggle-sidebar, open-settings, go-back, filter-all, filter-pinned, new-note, stop-recording, toggle-chat, pin-session, delete-session.

### `hooks/useGlobalShortcuts.ts`

```typescript
export function useGlobalShortcuts(): void
```

Global shortcuts via `@tauri-apps/plugin-global-shortcut`. Mounted in `App.tsx`. Subscribes to both `shortcutBindings` and `dictation.slots` — re-registers all bindings when either changes. `buildGlobalBindingMap()` merges static global shortcuts with dynamic dictation slot bindings. Dictation shortcuts dispatch `yapstack:dictation-start` (Pressed) / `yapstack:dictation-stop` (Released) events with `{ slotId }` detail. Toggle mode uses module-level `toggleActiveSlots: Set<string>` to track active toggle dictation sessions. Listens for `yapstack:dictation-idle` to clean up toggle state.

### `hooks/useDictation.ts`

```typescript
export function useDictation(): void
```

Voice dictation lifecycle (hold-to-talk or toggle mode). Mounted in `App.tsx` (main window only). Manages state machine: `idle → recording → transcribing → processing → done → idle`, plus a `cancelling` branch reachable from any non-idle phase via `yapstack:dictation-cancel`. Listens for `yapstack:dictation-start`, `yapstack:dictation-stop`, and `yapstack:dictation-cancel` custom events. On start: spins up a live transcription against a synthetic per-dictation session id so backfill, VAD chunking, and the streaming session WAV all reuse the live pipeline. On stop: waits for the loop's `session-part-ready` event to surface the finalized part path (WAV or MP3 per `audioExportFormat`), persists it to `dictation_history`, optionally runs AI post-processing, then routes output per `slot.outputAction`. The captured audio file is preserved regardless of Output action — only an explicit cancel deletes it. On cancel: aborts in-flight AI, stops live transcription, deletes the finalized part via `delete_audio_files`, suppresses Output action and Dictation history write — see ARCHITECTURE.md § "Cancellation". Registers Escape as a Global hotkey (via `@tauri-apps/plugin-global-shortcut`) for the lifetime of a non-idle Dictation; the hotkey dispatches `yapstack:dictation-cancel`. Controls the dictation bubble window position and visibility. Includes no-input detection (3s timer → `"no-input"` bubble state). Correlates parts to dictation entries via the synthetic session id on the `session-part-ready` event.

### `hooks/useRecordingIndicator.ts`

```typescript
export function useRecordingIndicator(): void
```

Shows/hides the recording indicator overlay window based on `activeSessionId`, `showRecordingIndicator` setting, and main window focus state. Positions the window at middle-right of the screen on first show. Listens for `recording-indicator:open-main` events from the indicator window and navigates to the active session. Mounted in `App.tsx` (`MainApp`).

### `hooks/useTrayEvents.ts`

```typescript
export function useTrayEvents(): void
```

Listens for tray menu Tauri events: `tray:new-session` (with backfill seconds payload), `tray:new-session-all` (max available buffer), `tray:stop-session`. Guards: engine must be ready, capture must be active, no active session for new-session events. Mounted in `AppLayout`.

---

## Dictation Frontend Types

### `stores/appStore.ts`

**Types**
| Type | Definition |
|------|-----------|
| `DictationActivationMode` | `"hold" \| "toggle"` |
| `DictationOutputAction` | `"paste" \| "clipboard" \| "new-note"` |
| `DictationSlot` | `{ id: string, name: string, enabled: boolean, aiEnabled: boolean, prompt: string, outputAction: DictationOutputAction }` |
| `DictationSettings` | `{ enabled: boolean, slots: DictationSlot[], activationMode: DictationActivationMode }` |

**Defaults**
| Export | Value |
|--------|-------|
| `DEFAULT_DICTATION_SLOTS` | `[{ id: "1", name: "Raw Dictation", enabled: true, aiEnabled: false, prompt: "", outputAction: "paste" }]` |
| `DEFAULT_DICTATION_SETTINGS` | `{ enabled: true, slots: DEFAULT_DICTATION_SLOTS, activationMode: "hold" }` |

### `lib/utils.ts`

**Functions**
| Function | Signature | Description |
|----------|-----------|-------------|
| `formatShortcutDisplay` | `(binding: string) → string` | Converts `"mod+shift+n"` to platform-specific display (⌘⇧N on macOS, Ctrl+Shift+N on Windows) |
