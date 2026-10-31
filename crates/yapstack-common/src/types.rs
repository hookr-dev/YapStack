use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub confidence: f32,
    /// Speaker ID assigned by diarization (0..N). `None` when diarization
    /// was not requested or is unsupported by the active engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_id: Option<u8>,
}

/// Which transcription engine should handle a request, and which the
/// frontend has selected. Engines are first-class peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    Whisper,
    Parakeet,
}

impl EngineKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EngineKind::Whisper => "whisper",
            EngineKind::Parakeet => "parakeet",
        }
    }
}

// --- Sidecar IPC Protocol ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SidecarRequest {
    #[serde(rename = "transcribe")]
    Transcribe {
        id: u64,
        audio_path: PathBuf,
        language: Option<String>,
        #[serde(default)]
        initial_prompt: Option<String>,
        /// Override single_segment mode. If None, the sidecar decides based on
        /// audio duration (true for <10s, false for longer chunks).
        #[serde(default)]
        single_segment: Option<bool>,
        /// Run speaker diarization in addition to transcription. Only honored
        /// when the active engine supports it (Parakeet). Whisper sidecar
        /// ignores this flag.
        #[serde(default)]
        diarization: bool,
    },
    #[serde(rename = "load_model")]
    LoadModel { id: u64, model_path: PathBuf },
    /// Query the backend for info about the currently loaded model
    /// (resolved accel + model_dir). Used at desktop spawn time to
    /// surface "what's running" — the initial model is loaded from the
    /// `--model` CLI arg before the IPC loop starts, so its
    /// `model_loaded` message is never sent over the wire. This request
    /// is cheap (cached state read on the sidecar side).
    #[serde(rename = "query_engine_info")]
    QueryEngineInfo { id: u64 },
    #[serde(rename = "shutdown")]
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SidecarResponse {
    #[serde(rename = "transcription")]
    Transcription {
        id: u64,
        text: String,
        segments: Vec<TranscriptSegment>,
        duration_ms: u64,
    },
    #[serde(rename = "model_loaded")]
    ModelLoaded {
        id: u64,
        /// Identifier of the execution provider that actually loaded the
        /// model — `"webgpu"`, `"coreml"`, `"cuda"`, or `"cpu"`. Reflects
        /// the *resolved* path including any fallback (e.g. WebGPU → CPU
        /// when EP init fails). None for engines that don't expose this
        /// (e.g. older sidecars — backward compat).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accel: Option<String>,
        /// Resolved model directory or file path the backend actually
        /// loaded from. Combined with `accel`, lets the desktop layer
        /// surface "what's running" without re-deriving from FE state.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_dir: Option<String>,
        /// Why the attempted accelerator failed to load, when `accel` is
        /// `"cpu"` because of a fallback rather than by choice. Additive:
        /// older sidecars omit it (deserializes to None) and older hosts
        /// ignore it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accel_fallback: Option<String>,
        /// `accel` is a GPU EP that leaves part of the model's heavy ops on
        /// the CPU (from the sidecar's static placement table). Additive:
        /// absent means false, and it is only sent when true.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        accel_partial: bool,
    },
    /// Response to `QueryEngineInfo` — same shape as `ModelLoaded` but
    /// fired in response to an explicit query, not a load. `accel`/
    /// `model_dir` are `None` when no model has been loaded yet.
    #[serde(rename = "engine_info")]
    EngineInfo {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accel: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_dir: Option<String>,
        /// See `ModelLoaded::accel_fallback`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accel_fallback: Option<String>,
        /// See `ModelLoaded::accel_partial`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        accel_partial: bool,
    },
    #[serde(rename = "error")]
    Error { id: u64, message: String },
    #[serde(rename = "progress")]
    Progress { id: u64, percent: f32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceType {
    Input,
    Output,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDeviceInfo {
    pub id: Option<String>,
    pub name: String,
    pub device_type: DeviceType,
    pub is_default: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureState {
    Idle,
    Capturing,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureStatus {
    pub state: CaptureState,
    pub mic_active: bool,
    pub system_audio_active: bool,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureSource {
    MicOnly,
    SystemOnly,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionStatus {
    Granted,
    Denied,
    NotDetermined,
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_device_info_serde_roundtrip() {
        let device = AudioDeviceInfo {
            id: Some("CoreAudio:BuiltInMicrophoneDevice".to_string()),
            name: "Built-in Microphone".to_string(),
            device_type: DeviceType::Input,
            is_default: true,
        };
        let json = serde_json::to_string(&device).unwrap();
        let deserialized: AudioDeviceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, device.id);
        assert_eq!(deserialized.name, device.name);
        assert_eq!(deserialized.device_type, device.device_type);
        assert_eq!(deserialized.is_default, device.is_default);

        // Backward compat: id=None round-trips correctly
        let device_no_id = AudioDeviceInfo {
            id: None,
            name: "Test".to_string(),
            device_type: DeviceType::Output,
            is_default: false,
        };
        let json2 = serde_json::to_string(&device_no_id).unwrap();
        let d2: AudioDeviceInfo = serde_json::from_str(&json2).unwrap();
        assert_eq!(d2.id, None);
    }

    #[test]
    fn test_device_type_serde_roundtrip() {
        for dt in [DeviceType::Input, DeviceType::Output] {
            let json = serde_json::to_string(&dt).unwrap();
            let deserialized: DeviceType = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, dt);
        }
    }

    #[test]
    fn test_capture_state_serde_roundtrip() {
        for state in [
            CaptureState::Idle,
            CaptureState::Capturing,
            CaptureState::Error,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let deserialized: CaptureState = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, state);
        }
    }

    #[test]
    fn test_capture_status_serde_roundtrip() {
        let status = CaptureStatus {
            state: CaptureState::Capturing,
            mic_active: true,
            system_audio_active: false,
            error_message: None,
        };
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: CaptureStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.state, status.state);
        assert_eq!(deserialized.mic_active, status.mic_active);
        assert_eq!(deserialized.system_audio_active, status.system_audio_active);
        assert_eq!(deserialized.error_message, status.error_message);

        let status_with_error = CaptureStatus {
            state: CaptureState::Error,
            mic_active: false,
            system_audio_active: false,
            error_message: Some("device disconnected".to_string()),
        };
        let json = serde_json::to_string(&status_with_error).unwrap();
        let deserialized: CaptureStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.error_message, status_with_error.error_message);
    }

    #[test]
    fn test_sidecar_request_transcribe_serde() {
        let req = SidecarRequest::Transcribe {
            id: 1,
            audio_path: PathBuf::from("/tmp/test.wav"),
            language: Some("en".to_string()),
            initial_prompt: None,
            single_segment: None,
            diarization: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"transcribe\""));
        let deserialized: SidecarRequest = serde_json::from_str(&json).unwrap();
        match deserialized {
            SidecarRequest::Transcribe { id, audio_path, .. } => {
                assert_eq!(id, 1);
                assert_eq!(audio_path, PathBuf::from("/tmp/test.wav"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_sidecar_request_transcribe_with_initial_prompt() {
        let req = SidecarRequest::Transcribe {
            id: 2,
            audio_path: PathBuf::from("/tmp/test.wav"),
            language: Some("en".to_string()),
            initial_prompt: Some("Hello world".to_string()),
            single_segment: None,
            diarization: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"initial_prompt\":\"Hello world\""));
        let deserialized: SidecarRequest = serde_json::from_str(&json).unwrap();
        match deserialized {
            SidecarRequest::Transcribe {
                id, initial_prompt, ..
            } => {
                assert_eq!(id, 2);
                assert_eq!(initial_prompt, Some("Hello world".to_string()));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_sidecar_request_transcribe_backward_compat() {
        // Deserialize without initial_prompt or diarization fields — both default.
        let json = r#"{"type":"transcribe","id":3,"audio_path":"/tmp/test.wav","language":"en"}"#;
        let deserialized: SidecarRequest = serde_json::from_str(json).unwrap();
        match deserialized {
            SidecarRequest::Transcribe {
                id,
                initial_prompt,
                diarization,
                ..
            } => {
                assert_eq!(id, 3);
                assert_eq!(initial_prompt, None);
                assert!(!diarization);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_model_loaded_accel_fallback_is_backward_compatible() {
        // Older sidecar: no accel_fallback key → None.
        let json = r#"{"type":"model_loaded","id":1,"accel":"cpu","model_dir":"/m"}"#;
        match serde_json::from_str::<SidecarResponse>(json).unwrap() {
            SidecarResponse::ModelLoaded {
                accel_fallback,
                accel_partial,
                ..
            } => {
                assert_eq!(accel_fallback, None);
                assert!(!accel_partial);
            }
            _ => panic!("wrong variant"),
        }
        let json = r#"{"type":"engine_info","id":2,"accel":"cpu"}"#;
        match serde_json::from_str::<SidecarResponse>(json).unwrap() {
            SidecarResponse::EngineInfo { accel_fallback, .. } => {
                assert_eq!(accel_fallback, None)
            }
            _ => panic!("wrong variant"),
        }

        // A normal load omits the key, so the wire stays what older hosts saw.
        let normal = SidecarResponse::ModelLoaded {
            id: 3,
            accel: Some("webgpu".into()),
            model_dir: None,
            accel_fallback: None,
            accel_partial: false,
        };
        let wire = serde_json::to_string(&normal).unwrap();
        assert!(!wire.contains("accel_fallback"));
        assert!(!wire.contains("accel_partial"));

        // A fallback round-trips its reason.
        let fallback = SidecarResponse::EngineInfo {
            id: 4,
            accel: Some("cpu".into()),
            model_dir: None,
            accel_fallback: Some("webgpu failed to load: no adapter".into()),
            accel_partial: false,
        };
        let json = serde_json::to_string(&fallback).unwrap();
        match serde_json::from_str::<SidecarResponse>(&json).unwrap() {
            SidecarResponse::EngineInfo { accel_fallback, .. } => assert_eq!(
                accel_fallback.as_deref(),
                Some("webgpu failed to load: no adapter")
            ),
            _ => panic!("wrong variant"),
        }

        // A partial GPU load round-trips its flag.
        let partial = SidecarResponse::ModelLoaded {
            id: 5,
            accel: Some("webgpu".into()),
            model_dir: None,
            accel_fallback: None,
            accel_partial: true,
        };
        let json = serde_json::to_string(&partial).unwrap();
        match serde_json::from_str::<SidecarResponse>(&json).unwrap() {
            SidecarResponse::ModelLoaded { accel_partial, .. } => assert!(accel_partial),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_sidecar_request_transcribe_with_diarization() {
        let req = SidecarRequest::Transcribe {
            id: 4,
            audio_path: PathBuf::from("/tmp/test.wav"),
            language: None,
            initial_prompt: None,
            single_segment: None,
            diarization: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"diarization\":true"));
        let deserialized: SidecarRequest = serde_json::from_str(&json).unwrap();
        match deserialized {
            SidecarRequest::Transcribe { diarization, .. } => assert!(diarization),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_sidecar_response_transcription_serde() {
        let resp = SidecarResponse::Transcription {
            id: 1,
            text: "hello world".to_string(),
            segments: vec![TranscriptSegment {
                start_ms: 0,
                end_ms: 1000,
                text: "hello world".to_string(),
                confidence: 0.95,
                speaker_id: None,
            }],
            duration_ms: 1000,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"type\":\"transcription\""));
        let deserialized: SidecarResponse = serde_json::from_str(&json).unwrap();
        match deserialized {
            SidecarResponse::Transcription {
                id, text, segments, ..
            } => {
                assert_eq!(id, 1);
                assert_eq!(text, "hello world");
                assert_eq!(segments.len(), 1);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_transcript_segment_speaker_id_roundtrip() {
        let seg = TranscriptSegment {
            start_ms: 0,
            end_ms: 1000,
            text: "hi".to_string(),
            confidence: 0.9,
            speaker_id: Some(2),
        };
        let json = serde_json::to_string(&seg).unwrap();
        assert!(json.contains("\"speaker_id\":2"));
        let back: TranscriptSegment = serde_json::from_str(&json).unwrap();
        assert_eq!(back.speaker_id, Some(2));
    }

    #[test]
    fn test_transcript_segment_speaker_id_omitted_when_none() {
        let seg = TranscriptSegment {
            start_ms: 0,
            end_ms: 1000,
            text: "hi".to_string(),
            confidence: 0.9,
            speaker_id: None,
        };
        let json = serde_json::to_string(&seg).unwrap();
        assert!(!json.contains("speaker_id"));
    }

    #[test]
    fn test_transcript_segment_backward_compat_no_speaker_id() {
        // Pre-Parakeet sidecars never emit speaker_id; deserialization must accept absence.
        let json = r#"{"start_ms":0,"end_ms":1000,"text":"hi","confidence":0.9}"#;
        let seg: TranscriptSegment = serde_json::from_str(json).unwrap();
        assert_eq!(seg.speaker_id, None);
    }

    #[test]
    fn test_engine_kind_serde() {
        assert_eq!(
            serde_json::to_string(&EngineKind::Whisper).unwrap(),
            "\"whisper\""
        );
        assert_eq!(
            serde_json::to_string(&EngineKind::Parakeet).unwrap(),
            "\"parakeet\""
        );
        let back: EngineKind = serde_json::from_str("\"parakeet\"").unwrap();
        assert_eq!(back, EngineKind::Parakeet);
    }

    #[test]
    fn test_sidecar_request_shutdown_serde() {
        let req = SidecarRequest::Shutdown;
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"shutdown\""));
        let deserialized: SidecarRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, SidecarRequest::Shutdown));
    }

    #[test]
    fn test_permission_status_serde_roundtrip() {
        for status in [
            PermissionStatus::Granted,
            PermissionStatus::Denied,
            PermissionStatus::NotDetermined,
            PermissionStatus::Unavailable,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let deserialized: PermissionStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, status);
        }
    }
}
