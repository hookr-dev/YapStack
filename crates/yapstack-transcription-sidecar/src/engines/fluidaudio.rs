//! Parakeet TDT v3 on CoreML through FluidAudio (ADR 0008), via the C-ABI
//! shim in `swift/YapFluidAudio` that build.rs builds and links on Apple
//! silicon. Models load from a local folder of FluidAudio's compiled
//! `.mlmodelc` bundles; nothing is downloaded at run time. The shim always
//! loads the int8 per-channel `Encoder_v2`.

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::Path;

use parakeet_rs::TimedToken;
use serde::Deserialize;

extern "C" {
    fn yfa_load(dir: *const c_char, units: i32, err: *mut *mut c_char) -> *mut c_void;
    fn yfa_transcribe(
        handle: *mut c_void,
        samples: *const f32,
        count: usize,
        json: *mut *mut c_char,
    ) -> i32;
    fn yfa_encoder_compute_plan(dir: *const c_char, units: i32, json: *mut *mut c_char) -> i32;
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

fn c_path(dir: &Path) -> Result<CString, String> {
    CString::new(dir.display().to_string()).map_err(|e| e.to_string())
}

/// The CoreML compute units every model of a load gets (FluidAudio pins the
/// preprocessor to the CPU either way).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputeUnits {
    CpuAndNeuralEngine,
    CpuOnly,
}

impl ComputeUnits {
    /// The shim's code for it.
    fn raw(self) -> i32 {
        match self {
            ComputeUnits::CpuAndNeuralEngine => 0,
            ComputeUnits::CpuOnly => 1,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Word {
    w: String,
    s: f64,
    e: f64,
}

#[derive(Debug, Deserialize)]
struct Output {
    words: Vec<Word>,
}

pub struct FluidAudioModel {
    handle: *mut c_void,
}

// SAFETY: the handle is a retained Swift object wrapping an `AsrManager`
// actor; every call into it is serialized by the actor and by `&mut self`.
unsafe impl Send for FluidAudioModel {}

impl FluidAudioModel {
    pub fn load(dir: &Path, units: ComputeUnits) -> Result<Self, String> {
        let c_dir = c_path(dir)?;
        let mut err: *mut c_char = std::ptr::null_mut();
        // SAFETY: valid C string; `err` is an out-pointer.
        let handle = unsafe { yfa_load(c_dir.as_ptr(), units.raw(), &mut err) };
        if handle.is_null() {
            return Err(format!(
                "failed to load parakeet CoreML model: {}",
                take_string(err)
            ));
        }
        Ok(Self { handle })
    }

    /// Word timings of 16 kHz mono audio, in the shape parakeet-rs's
    /// `TimestampMode::Words` returns, so the ORT engine's segment grouping
    /// applies unchanged.
    pub fn transcribe(&mut self, mono_16k: &[f32]) -> Result<Vec<TimedToken>, String> {
        let mut json: *mut c_char = std::ptr::null_mut();
        // SAFETY: the slice outlives the call; `json` is an out-pointer.
        let rc =
            unsafe { yfa_transcribe(self.handle, mono_16k.as_ptr(), mono_16k.len(), &mut json) };
        let body = take_string(json);
        if rc != 0 {
            return Err(format!("parakeet CoreML transcription failed: {body}"));
        }
        tokens_from_output(&body, mono_16k.len() as f32 / 16_000.0)
    }
}

/// The shim's JSON result as word tokens of a chunk `audio_secs` long.
/// FluidAudio pads the audio to whole 80 ms encoder frames and adds the TDT
/// duration to the last token's end, so its times can run past the audio;
/// parakeet-rs does neither. Clamping to the chunk keeps both runtimes'
/// timestamps inside the audio they came from.
fn tokens_from_output(body: &str, audio_secs: f32) -> Result<Vec<TimedToken>, String> {
    let out: Output =
        serde_json::from_str(body).map_err(|e| format!("parakeet CoreML result parse: {e}"))?;
    let clamp = |t: f64| (t as f32).clamp(0.0, audio_secs);
    Ok(out
        .words
        .into_iter()
        .map(|w| TimedToken {
            text: w.w,
            start: clamp(w.s),
            end: clamp(w.e),
        })
        .collect())
}

impl Drop for FluidAudioModel {
    fn drop(&mut self) {
        // SAFETY: handle came from `yfa_load` and is released once.
        unsafe { yfa_destroy(self.handle) };
    }
}

/// Where CoreML plans to run each operation: MIL operator name → preferred
/// device (`"ane"`, `"cpu"`, `"gpu"`, `"none"`) → count.
pub type ComputePlan = HashMap<String, HashMap<String, usize>>;

/// `MLComputePlan` of the encoder `FluidAudioModel::load` loads from `dir`
/// under `units`. Fails before macOS 14.4, which has no `MLComputePlan`.
pub fn encoder_compute_plan(dir: &Path, units: ComputeUnits) -> Result<ComputePlan, String> {
    let c_dir = c_path(dir)?;
    let mut json: *mut c_char = std::ptr::null_mut();
    // SAFETY: valid C string; `json` is an out-pointer.
    let rc = unsafe { yfa_encoder_compute_plan(c_dir.as_ptr(), units.raw(), &mut json) };
    let body = take_string(json);
    if rc != 0 {
        return Err(body);
    }
    serde_json::from_str(&body).map_err(|e| format!("compute plan parse: {e}"))
}

/// The MIL operators that carry the encoder's matmul and conv work: Gate
/// G4's "heavy" ops in CoreML terms. A plan names them with their opset
/// (`ios17.linear`); these are the bare names.
const HEAVY_OPS: [&str; 4] = ["conv", "matmul", "linear", "einsum"];

/// `ios17.linear` → `linear`.
fn bare_op_name(op: &str) -> &str {
    op.rsplit('.').next().unwrap_or(op)
}

/// The accelerator label for a CoreML load and whether some heavy ops run
/// off it, from the encoder's compute plan (Gate G4's truthful label):
/// `"ane"` only when the plan puts heavy ops on the Neural Engine (partial
/// when some of them run elsewhere), `"cpu"` when it puts none there, and
/// `"coreml"` when there is no plan, or no heavy op in it, to go by.
pub fn accel_from_plan(plan: Option<&ComputePlan>) -> (&'static str, bool) {
    let Some(plan) = plan else {
        return ("coreml", false);
    };
    let (mut on_ane, mut total) = (0, 0);
    for (op, devices) in plan {
        if HEAVY_OPS.contains(&bare_op_name(op)) {
            on_ane += devices.get("ane").copied().unwrap_or(0);
            total += devices.values().sum::<usize>();
        }
    }
    match (on_ane, total) {
        (_, 0) => ("coreml", false),
        (0, _) => ("cpu", false),
        (ane, total) => ("ane", ane < total),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(entries: &[(&str, &str, usize)]) -> ComputePlan {
        let mut plan = ComputePlan::new();
        for (op, device, n) in entries {
            *plan
                .entry(op.to_string())
                .or_default()
                .entry(device.to_string())
                .or_default() += n;
        }
        plan
    }

    fn spans(tokens: &[TimedToken]) -> Vec<(&str, f32, f32)> {
        tokens
            .iter()
            .map(|t| (t.text.as_str(), t.start, t.end))
            .collect()
    }

    #[test]
    fn the_shim_result_maps_to_word_tokens() {
        let body = r#"{"words":[{"w":"Hello","s":0.08,"e":0.4},{"w":"world.","s":0.48,"e":0.96}]}"#;
        let tokens = tokens_from_output(body, 2.0).unwrap();
        assert_eq!(
            spans(&tokens),
            [("Hello", 0.08, 0.4), ("world.", 0.48, 0.96)]
        );
        assert!(tokens_from_output("{}", 2.0).is_err());
    }

    /// FluidAudio pads the audio to whole 80 ms encoder frames and adds the
    /// TDT duration to the last token's end, so the last word can end past
    /// the audio (up to 161 ms on the AE-G1 replay). parakeet-rs does
    /// neither; times are clamped to the chunk so both runtimes agree.
    #[test]
    fn word_times_are_clamped_to_the_chunk() {
        let body = r#"{"words":[{"w":"a","s":0.5,"e":1.2},{"w":"b","s":1.98,"e":2.16},{"w":"c","s":2.08,"e":2.24}]}"#;
        let tokens = tokens_from_output(body, 2.0).unwrap();
        assert_eq!(
            spans(&tokens),
            [("a", 0.5, 1.2), ("b", 1.98, 2.0), ("c", 2.0, 2.0)]
        );
    }

    #[test]
    fn heavy_ops_all_on_the_ane_are_labelled_ane() {
        let p = plan(&[
            ("conv", "ane", 24),
            ("linear", "ane", 300),
            ("add", "cpu", 4),
        ]);
        assert_eq!(accel_from_plan(Some(&p)), ("ane", false));
    }

    /// The shape of the real Encoder_v2 plan (M5 Pro, macOS 27): opset
    /// prefixes, dequantize ops with no device, two casts on the CPU.
    #[test]
    fn opset_prefixed_op_names_are_matched() {
        let p = plan(&[
            ("ios17.conv", "ane", 77),
            ("ios17.linear", "ane", 193),
            ("ios17.matmul", "ane", 72),
            ("ios17.cast", "cpu", 2),
            ("ios16.constexpr_affine_dequantize", "none", 294),
        ]);
        assert_eq!(accel_from_plan(Some(&p)), ("ane", false));
        let p = plan(&[("ios17.linear", "ane", 192), ("ios17.linear", "cpu", 1)]);
        assert_eq!(accel_from_plan(Some(&p)), ("ane", true));
    }

    #[test]
    fn a_heavy_op_off_the_ane_makes_it_partial() {
        let p = plan(&[("linear", "ane", 299), ("linear", "cpu", 1)]);
        assert_eq!(accel_from_plan(Some(&p)), ("ane", true));
    }

    #[test]
    fn no_heavy_op_on_the_ane_is_cpu() {
        let p = plan(&[
            ("conv", "cpu", 24),
            ("matmul", "cpu", 10),
            ("add", "ane", 5),
        ]);
        assert_eq!(accel_from_plan(Some(&p)), ("cpu", false));
    }

    #[test]
    fn without_a_plan_the_ane_is_never_claimed() {
        assert_eq!(accel_from_plan(None), ("coreml", false));
        let p = plan(&[("add", "ane", 5), ("const", "none", 100)]);
        assert_eq!(accel_from_plan(Some(&p)), ("coreml", false));
    }
}
