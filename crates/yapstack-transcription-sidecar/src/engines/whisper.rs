//! Whisper backend (whisper-rs 0.15 + Metal/Flash-Attention on macOS).

use std::path::{Path, PathBuf};

use tracing::{debug, info};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperVadParams,
};
use yapstack_common::types::TranscriptSegment;

use crate::engines::{
    normalize_spacing, read_wav_as_mono_16k, sanitize_text, should_include_segment, EngineInfo,
    TranscribeOpts, TranscriptionBackend, TranscriptionOutput,
};

pub struct WhisperBackend {
    ctx: Option<WhisperContext>,
    /// Optional Silero VAD model path (passed to whisper.cpp at transcribe time).
    /// Set once at sidecar startup via `--vad-model` and reused for every request.
    vad_model_path: Option<PathBuf>,
    /// Set after each successful `load_model`. Whisper has a single
    /// EP path (Metal when compiled in, CPU otherwise) so `accel`
    /// reports the cfg-determined label.
    last_engine_info: Option<EngineInfo>,
}

impl WhisperBackend {
    pub fn new(vad_model_path: Option<PathBuf>) -> Self {
        Self {
            ctx: None,
            vad_model_path,
            last_engine_info: None,
        }
    }
}

impl TranscriptionBackend for WhisperBackend {
    fn load_model(&mut self, model_path: &Path) -> Result<(), String> {
        let mut ctx_params = WhisperContextParameters::default();
        ctx_params.flash_attn(true);

        info!(
            "whisper context: use_gpu={}, flash_attn={}, gpu_device={}",
            ctx_params.use_gpu, ctx_params.flash_attn, ctx_params.gpu_device
        );

        let ctx = WhisperContext::new_with_params(
            model_path.to_str().ok_or("invalid model path")?,
            ctx_params,
        )
        .map_err(|e| format!("failed to load model: {e}"))?;
        self.ctx = Some(ctx);
        // Whisper EP is fixed at compile time — Metal on Apple builds,
        // CPU everywhere else. Report it so the desktop layer can
        // surface a "Whisper · Metal" badge symmetrically with Parakeet.
        let accel = if cfg!(feature = "metal") {
            "metal".to_string()
        } else {
            "cpu".to_string()
        };
        self.last_engine_info = Some(EngineInfo {
            accel,
            model_dir: model_path.display().to_string(),
            accel_fallback: None,
            accel_partial: false,
        });
        Ok(())
    }

    fn engine_info(&self) -> Option<EngineInfo> {
        self.last_engine_info.clone()
    }

    fn transcribe(
        &mut self,
        audio_path: &Path,
        opts: TranscribeOpts<'_>,
    ) -> Result<TranscriptionOutput, String> {
        let ctx = self.ctx.as_ref().ok_or("no model loaded")?;

        let samples = read_wav_as_mono_16k(audio_path)?;

        let start = std::time::Instant::now();

        let n_threads = std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(4);

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        if let Some(lang) = opts.language {
            params.set_language(Some(lang));
        }
        if let Some(prompt) = opts.initial_prompt {
            params.set_initial_prompt(prompt);
        }
        params.set_n_threads(n_threads);
        params.set_temperature(0.0);
        params.set_no_speech_thold(0.45);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);

        // Temperature fallback: greedy → +0.2 on failure. whisper.cpp retries
        // when a segment has high entropy or low avg logprob, recovering speech
        // from noisy/ambiguous audio.
        params.set_temperature_inc(0.2);

        // Reject segments with repetitive/compressible text (default 2.4).
        params.set_entropy_thold(2.0);

        // Use whisper.cpp default; -0.5 was too aggressive and pre-rejected
        // legitimate segments before our post-filter could see them.
        params.set_logprob_thold(-1.0);

        // Adaptive single_segment: short chunks (<10s) → single segment to
        // avoid over-segmentation; longer chunks → natural sentence boundaries.
        let audio_duration_s = samples.len() as f32 / 16_000.0;
        let use_single_segment = opts.single_segment.unwrap_or(audio_duration_s < 10.0);
        params.set_single_segment(use_single_segment);

        // Cap decoder output per segment to prevent runaway repetition loops.
        params.set_max_tokens(if use_single_segment { 100 } else { 200 });

        debug!(
            "whisper params: n_threads={}, best_of=1, temperature=0.0, temperature_inc=0.2, no_speech_thold=0.45, entropy_thold=2.0, logprob_thold=-1.0, single_segment={}, max_tokens={}",
            n_threads,
            use_single_segment,
            if use_single_segment { 100 } else { 200 },
        );

        // We pass curated initial_prompt from the live controller; disable
        // whisper.cpp's own prior-context feedback to avoid amplifying hallucinations.
        params.set_no_context(true);

        if let Some(vad_path) = self.vad_model_path.as_deref() {
            if let Some(vad_str) = vad_path.to_str() {
                params.set_vad_model_path(Some(vad_str));
                let mut vad_params = WhisperVadParams::new();
                vad_params.set_threshold(0.5);
                vad_params.set_min_speech_duration(250);
                vad_params.set_min_silence_duration(100);
                vad_params.set_speech_pad(30);
                params.set_vad_params(vad_params);
                params.enable_vad(true);
                info!("VAD enabled for transcription");
            } else {
                info!("VAD model path contains non-UTF-8 characters, skipping VAD");
            }
        }

        let mut state = ctx
            .create_state()
            .map_err(|e| format!("failed to create state: {e}"))?;

        state
            .full(params, &samples)
            .map_err(|e| format!("transcription failed: {e}"))?;

        let num_segments = state.full_n_segments();
        let mut text = String::new();
        let mut segments = Vec::new();

        for i in 0..num_segments {
            let segment = state
                .get_segment(i)
                .ok_or_else(|| format!("segment {i} out of bounds"))?;
            let segment_text = segment
                .to_str()
                .map_err(|e| format!("failed to get segment text: {e}"))?
                .to_string();
            let start_ts = segment.start_timestamp();
            let end_ts = segment.end_timestamp();
            let confidence = 1.0 - segment.no_speech_probability();

            if !should_include_segment(
                &segment_text,
                confidence,
                yapstack_common::types::EngineKind::Whisper,
            ) {
                info!(
                    len = segment_text.trim().chars().count(),
                    confidence = confidence,
                    "hallucination filtered"
                );
                continue;
            }

            let normalized = normalize_spacing(segment_text.trim());
            let sanitized = sanitize_text(&normalized);
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&sanitized);

            segments.push(TranscriptSegment {
                start_ms: (start_ts * 10) as u64,
                end_ms: (end_ts * 10) as u64,
                text: sanitized,
                confidence,
                speaker_id: None,
            });
        }

        let duration_ms = start.elapsed().as_millis() as u64;
        Ok(TranscriptionOutput {
            text,
            segments,
            duration_ms,
        })
    }
}
