//! SPIKE (AE3): Parakeet TDT v3 on CoreML / the Neural Engine through
//! FluidAudio, via the C-ABI shim in `swift/YapFluidAudio` (built by
//! build.rs). Models load from a local directory of FluidAudio's compiled
//! `.mlmodelc` bundles; nothing is downloaded at run time.
//!
//! Run-time knobs (spike only):
//! - `YAPSTACK_FLUIDAUDIO_UNITS`: `ane` (default, CPU + Neural Engine),
//!   `gpu` (CPU + GPU), `cpu`, `all`. Applies to decoder/joint/encoder;
//!   FluidAudio pins the preprocessor to CPU.
//! - `YAPSTACK_FLUIDAUDIO_ENCODER`: `int8` (default, `Encoder.mlmodelc`) or
//!   `int8-v2` (`Encoder_v2.mlmodelc`).
//! - `YAPSTACK_FLUIDAUDIO_PLAN=1`: log `MLComputePlan` per-op device counts
//!   for the encoder at load.

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::Path;

use parakeet_rs::TimedToken;
use serde::Deserialize;
use tracing::info;

extern "C" {
    fn yfa_load(
        dir: *const c_char,
        encoder: *const c_char,
        units: i32,
        err: *mut *mut c_char,
    ) -> *mut c_void;
    fn yfa_transcribe(
        handle: *mut c_void,
        samples: *const f32,
        count: usize,
        json: *mut *mut c_char,
    ) -> i32;
    fn yfa_compute_plan(path: *const c_char, units: i32, json: *mut *mut c_char) -> i32;
    fn yfa_destroy(handle: *mut c_void);
    fn yfa_free_string(s: *mut c_char);
}

/// Takes ownership of a shim-allocated string.
fn take_string(p: *mut c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: the shim returns NUL-terminated strdup'd UTF-8 strings.
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { yfa_free_string(p) };
    s
}

#[derive(Debug, Deserialize)]
struct Word {
    w: String,
    s: f64,
    e: f64,
}

#[derive(Debug, Deserialize)]
pub struct Output {
    // Decoded for parity checks in logs; segments are rebuilt from `words`.
    #[allow(dead_code)]
    pub text: String,
    #[allow(dead_code)]
    pub confidence: f32,
    words: Vec<Word>,
}

impl Output {
    /// Word timings in the same shape parakeet-rs's `TimestampMode::Words`
    /// returns, so the ORT engine's segment grouping applies unchanged.
    pub fn words(&self) -> Vec<TimedToken> {
        self.words
            .iter()
            .map(|w| TimedToken {
                text: w.w.clone(),
                start: w.s as f32,
                end: w.e as f32,
            })
            .collect()
    }
}

pub struct FluidAudioModel {
    handle: *mut c_void,
}

// SAFETY: the handle is a retained Swift object wrapping an `AsrManager`
// actor; every call into it is serialized by the actor and by `&mut self`.
unsafe impl Send for FluidAudioModel {}

fn units_from_env() -> (i32, &'static str) {
    match std::env::var("YAPSTACK_FLUIDAUDIO_UNITS")
        .unwrap_or_default()
        .as_str()
    {
        "cpu" => (1, "cpu"),
        "gpu" => (2, "gpu"),
        "all" => (3, "all"),
        _ => (0, "ane"),
    }
}

impl FluidAudioModel {
    pub fn load(dir: &Path) -> Result<(Self, String), String> {
        let (units, units_label) = units_from_env();
        let encoder =
            std::env::var("YAPSTACK_FLUIDAUDIO_ENCODER").unwrap_or_else(|_| "int8".to_string());
        let c_dir = CString::new(dir.display().to_string()).map_err(|e| e.to_string())?;
        let c_enc = CString::new(encoder.clone()).map_err(|e| e.to_string())?;
        if std::env::var_os("YAPSTACK_FLUIDAUDIO_PLAN").is_some() {
            let file = if encoder == "int8-v2" {
                "Encoder_v2.mlmodelc"
            } else {
                "Encoder.mlmodelc"
            };
            log_compute_plan(&dir.join(file), units);
        }
        let mut err: *mut c_char = std::ptr::null_mut();
        // SAFETY: valid C strings; `err` is an out-pointer.
        let handle = unsafe { yfa_load(c_dir.as_ptr(), c_enc.as_ptr(), units, &mut err) };
        if handle.is_null() {
            return Err(format!("fluidaudio load failed: {}", take_string(err)));
        }
        Ok((
            Self { handle },
            format!("units={units_label} encoder={encoder}"),
        ))
    }

    pub fn transcribe(&mut self, mono_16k: &[f32]) -> Result<Output, String> {
        let mut json: *mut c_char = std::ptr::null_mut();
        // SAFETY: the slice outlives the call; `json` is an out-pointer.
        let rc =
            unsafe { yfa_transcribe(self.handle, mono_16k.as_ptr(), mono_16k.len(), &mut json) };
        let body = take_string(json);
        if rc != 0 {
            return Err(format!("fluidaudio transcription failed: {body}"));
        }
        serde_json::from_str(&body).map_err(|e| format!("fluidaudio result parse: {e}"))
    }
}

impl Drop for FluidAudioModel {
    fn drop(&mut self) {
        // SAFETY: handle came from `yfa_load` and is released once.
        unsafe { yfa_destroy(self.handle) };
    }
}

fn log_compute_plan(model: &Path, units: i32) {
    let Ok(c_path) = CString::new(model.display().to_string()) else {
        return;
    };
    let mut json: *mut c_char = std::ptr::null_mut();
    // SAFETY: valid C string; `json` is an out-pointer.
    let rc = unsafe { yfa_compute_plan(c_path.as_ptr(), units, &mut json) };
    let body = take_string(json);
    info!(
        marker = "fluidaudio_compute_plan",
        model = %model.display(),
        ok = rc == 0,
        plan = %body,
        "coreml compute plan"
    );
}
