//! Engine abstraction layer for the transcription sidecar.
//!
//! Each backend implements [`TranscriptionBackend`]. Parakeet is the only
//! backend today (ADR 0005); the trait stays as the seam for later models.
//! The sidecar is spawned with `--engine parakeet` and constructs the
//! matching backend; the IPC dispatch loop in `main.rs` is engine-agnostic.

#[cfg(feature = "parakeet")]
use std::borrow::Cow;
#[cfg(feature = "parakeet")]
use std::path::Path;

#[cfg(feature = "parakeet")]
use yapstack_common::audio::MonoResampler;

#[cfg(feature = "parakeet")]
use yapstack_common::types::TranscriptSegment;

#[cfg(all(feature = "fluidaudio", target_os = "macos"))]
pub mod fluidaudio;
#[cfg(feature = "parakeet")]
pub mod cpu_cores;
#[cfg(feature = "parakeet")]
pub mod ort_placement;
#[cfg(feature = "parakeet")]
pub mod parakeet;

/// Per-request transcription options. Engines ignore options they don't
/// support (Parakeet auto-detects the language and ignores `language`).
#[cfg(feature = "parakeet")]
#[derive(Debug, Clone, Copy)]
pub struct TranscribeOpts<'a> {
    pub language: Option<&'a str>,
    /// Honored by Parakeet via Sortformer.
    #[allow(dead_code)]
    pub diarization: bool,
}

#[cfg(feature = "parakeet")]
#[derive(Debug, Clone)]
pub struct TranscriptionOutput {
    pub text: String,
    pub segments: Vec<TranscriptSegment>,
    pub duration_ms: u64,
}

/// Read a WAV file, downmix to mono, and resample to 16 kHz. Parakeet
/// requires this exact input shape. A 16 kHz file is not resampled; any
/// other rate goes through `resampler`, which is rebuilt only when the
/// file's rate differs from the last one it served.
#[cfg(feature = "parakeet")]
pub(crate) fn read_wav_as_mono_16k(
    audio_path: &Path,
    resampler: &mut Option<MonoResampler>,
) -> Result<Vec<f32>, String> {
    let mut reader =
        hound::WavReader::open(audio_path).map_err(|e| format!("failed to open WAV: {e}"))?;
    let spec = reader.spec();
    let raw: Vec<f32> = if spec.sample_format == hound::SampleFormat::Float {
        reader
            .samples::<f32>()
            .map(|s| s.map_err(|e| format!("sample read error: {e}")))
            .collect::<Result<Vec<f32>, String>>()?
    } else {
        reader
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / i16::MAX as f32))
            .map(|s| s.map_err(|e| format!("sample read error: {e}")))
            .collect::<Result<Vec<f32>, String>>()?
    };
    let mono = match yapstack_common::audio::deinterleave_to_mono(&raw, spec.channels) {
        Cow::Borrowed(_) => raw,
        Cow::Owned(mono) => mono,
    };
    to_16k(mono, spec.sample_rate, resampler)
}

#[cfg(feature = "parakeet")]
fn to_16k(
    mono: Vec<f32>,
    sample_rate: u32,
    resampler: &mut Option<MonoResampler>,
) -> Result<Vec<f32>, String> {
    const TARGET: u32 = 16_000;
    if sample_rate == TARGET || mono.is_empty() {
        return Ok(mono);
    }
    let r = match resampler {
        Some(r) if r.rates() == (sample_rate, TARGET) => r,
        _ => resampler.insert(
            MonoResampler::new(sample_rate, TARGET)
                .map_err(|e| format!("resample to 16kHz failed: {e}"))?,
        ),
    };
    r.process(&mono)
        .map_err(|e| format!("resample to 16kHz failed: {e}"))
}

/// Information about the model that a backend has loaded, exposed back
/// to the desktop layer through `SidecarResponse::ModelLoaded` so the
/// frontend can surface "Parakeet · WebGPU" badges without re-deriving
/// from frontend state. `accel` reflects the *resolved* execution
/// provider including any runtime fallback (e.g. `"cpu"` when WebGPU
/// init fails and the load chain falls back).
#[cfg(feature = "parakeet")]
#[derive(Debug, Clone)]
pub struct EngineInfo {
    pub accel: String,
    pub model_dir: String,
    /// Why the attempted accelerator failed to load, when the load fell
    /// back to CPU. `None` when the attempted EP loaded, or when CPU was
    /// the resolved choice in the first place.
    pub accel_fallback: Option<String>,
    /// `accel` is a GPU EP that, per the sidecar's static placement table,
    /// leaves part of the model's heavy ops on the CPU EP.
    pub accel_partial: bool,
}

/// What every transcription backend must implement. Engine-specific
/// configuration (Parakeet's Sortformer model) is
/// passed to the backend's constructor — not through this trait.
#[cfg(feature = "parakeet")]
pub trait TranscriptionBackend: Send {
    /// (Re)load the primary transcription model from disk. May be called
    /// more than once per process if the user switches models.
    fn load_model(&mut self, model_path: &Path) -> Result<(), String>;

    /// Engine info from the most recent successful `load_model` call.
    /// `None` when no model is loaded yet, or when the backend doesn't
    /// expose this (default impl). Backends that select between EPs
    /// (currently just Parakeet) override to report which EP actually
    /// took the model — useful for telemetry / UI badges.
    fn engine_info(&self) -> Option<EngineInfo> {
        None
    }

    /// Transcribe the WAV file at `audio_path`. The backend is responsible
    /// for any required resampling/channel conversion. Takes `&mut self`
    /// because some backends (Parakeet) mutate decoder state per call.
    fn transcribe(
        &mut self,
        audio_path: &Path,
        opts: TranscribeOpts<'_>,
    ) -> Result<TranscriptionOutput, String>;
}

// ---------- Shared text post-processing ----------
//
// Mild filters: no-ops on clean text, so running every Parakeet segment
// through them is harmless.

/// Insert a space after sentence-ending punctuation (`.` `!` `?`) or commas
/// when immediately followed by an uppercase letter. Fixes joined sentences
/// like "First sentence.Then second" while preserving "e.g." abbreviations
/// (which are followed by lowercase).
#[cfg(any(feature = "parakeet", test))]
pub(crate) fn normalize_spacing(text: &str) -> String {
    let mut result = String::with_capacity(text.len() + 10);
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        result.push(ch);
        if ch == '.' || ch == '!' || ch == '?' || ch == ',' {
            if let Some(&next) = chars.peek() {
                if next.is_uppercase() {
                    result.push(' ');
                }
            }
        }
    }
    result
}

/// Replace common Unicode punctuation (smart quotes, em/en dashes, ellipsis)
/// with ASCII equivalents, strip control characters, and collapse runs of
/// multiple spaces. Other printable Unicode (accents, non-Latin scripts) is
/// preserved verbatim — Parakeet TDT v3 supports 25 European languages,
/// including non-ASCII scripts.
#[cfg(any(feature = "parakeet", test))]
pub(crate) fn sanitize_text(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut prev_space = false;
    for ch in text.chars() {
        match ch {
            '\u{2018}' | '\u{2019}' => {
                result.push('\'');
                prev_space = false;
            }
            '\u{201C}' | '\u{201D}' => {
                result.push('"');
                prev_space = false;
            }
            '\u{2014}' => {
                result.push_str("--");
                prev_space = false;
            }
            '\u{2013}' => {
                result.push('-');
                prev_space = false;
            }
            '\u{2026}' => {
                result.push_str("...");
                prev_space = false;
            }
            ' ' => {
                if !prev_space {
                    result.push(' ');
                }
                prev_space = true;
            }
            // Strip control + invisible-formatting noise (ZWSP, BOM) but keep
            // printable Unicode (accented Latin, Greek, Cyrillic, etc.).
            '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}' => {}
            c if c.is_control() => {}
            c => {
                result.push(c);
                prev_space = false;
            }
        }
    }
    result
}

/// Normalize text for repetition detection: insert spaces around punctuation
/// so "Yeah.Yeah.Yeah." becomes "Yeah . Yeah . Yeah ." and whitespace-based
/// splitting catches the loop.
#[cfg(any(feature = "parakeet", test))]
pub(crate) fn normalize_for_repetition(text: &str) -> String {
    let mut result = String::with_capacity(text.len() * 2);
    for ch in text.chars() {
        if ch.is_ascii_punctuation() && ch != '\'' {
            if !result.ends_with(' ') {
                result.push(' ');
            }
            result.push(ch);
            result.push(' ');
        } else {
            result.push(ch);
        }
    }
    result
}

/// Detect excessive repetition (`"the the the the the the"` or
/// `"thank you thank you …"`). Returns `true` when ≥6 consecutive identical
/// words or ≥6 repeats of a short phrase are found.
///
/// The 6-repeat threshold (raised from 3) lets real conversational
/// repetition like "no no no that's not what I meant" pass; the call site
/// in `should_include_segment` additionally gates on confidence < 0.7 so
/// high-confidence stutters survive.
#[cfg(any(feature = "parakeet", test))]
pub(crate) fn has_excessive_repetition(text: &str) -> bool {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < 6 {
        return false;
    }

    let mut consecutive = 1;
    for pair in words.windows(2) {
        if pair[0].eq_ignore_ascii_case(pair[1]) {
            consecutive += 1;
            if consecutive >= 6 {
                return true;
            }
        } else {
            consecutive = 1;
        }
    }

    for phrase_len in 1..=words.len() / 2 {
        let phrase = &words[..phrase_len];
        let all_repeat = words.chunks(phrase_len).all(|chunk| {
            chunk
                .iter()
                .zip(phrase.iter())
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        });
        if all_repeat && words.len() / phrase_len >= 6 {
            return true;
        }
    }

    false
}

/// Whether a transcript segment should be kept. Filters out empty text,
/// special tokens (`[BLANK_AUDIO]`), low-confidence segments, known YouTube
/// hallucination patterns, and excessive repetition.
///
/// The repetition gate only fires at confidence < 0.7 so legitimate
/// high-confidence stutters survive.
#[cfg(any(feature = "parakeet", test))]
pub(crate) fn should_include_segment(text: &str, confidence: f32) -> bool {
    let trimmed = text.trim();

    if trimmed.is_empty() {
        return false;
    }

    if !trimmed.chars().any(|c| c.is_alphanumeric()) {
        return false;
    }

    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        return false;
    }

    // Repetition: only filter at marginal confidence. Real speech repetitions
    // ("no no no no no no") at ≥0.7 confidence pass through; long hallucinated
    // loops are typically much longer than 6 repeats AND low confidence.
    if confidence < 0.7 {
        if has_excessive_repetition(trimmed) {
            tracing::info!(
                len = trimmed.chars().count(),
                confidence = confidence,
                "repetition filtered"
            );
            return false;
        }
        let normalized = normalize_for_repetition(trimmed);
        if has_excessive_repetition(&normalized) {
            tracing::info!(
                len = trimmed.chars().count(),
                normalized_len = normalized.chars().count(),
                confidence = confidence,
                "repetition filtered (normalized)"
            );
            return false;
        }
    }

    if confidence < 0.4 {
        return false;
    }

    if yapstack_common::hallucination::is_always_reject(trimmed) {
        return false;
    }

    if confidence < 0.6 && yapstack_common::hallucination::is_marginal_reject(trimmed) {
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn include_p(text: &str, confidence: f32) -> bool {
        should_include_segment(text, confidence)
    }

    #[test]
    fn normal_text_high_confidence_included() {
        assert!(include_p("Hello, how are you?", 0.9));
    }

    #[test]
    fn parakeet_thank_you_high_confidence_included() {
        // Demoted from always-reject to marginal — high-confidence "thank you"
        // is real polite speech on Parakeet.
        assert!(include_p("Thank you.", 0.95));
        assert!(include_p("Thank you!", 0.9));
    }

    #[test]
    fn parakeet_thank_you_low_confidence_excluded() {
        // Marginal tier still drops at confidence < 0.6.
        assert!(!include_p("Thank you.", 0.5));
        assert!(!include_p("Thank you.", 0.3));
    }

    #[test]
    fn empty_text_excluded() {
        assert!(!include_p("", 0.9));
        assert!(!include_p("   ", 0.9));
    }

    #[test]
    fn blank_audio_token_excluded() {
        assert!(!include_p("[BLANK_AUDIO]", 0.9));
    }

    #[test]
    fn parakeet_you_marginal_only() {
        // Demoted: high-confidence "you" passes on Parakeet.
        assert!(include_p("you", 0.9));
        assert!(!include_p("you", 0.5));
    }

    #[test]
    fn longer_sentence_with_you_included() {
        assert!(include_p("You should try this", 0.7));
    }

    #[test]
    fn below_confidence_threshold_excluded() {
        assert!(!include_p("Some random text", 0.35));
    }

    #[test]
    fn special_tokens_excluded() {
        assert!(!include_p("[MUSIC]", 0.9));
        assert!(!include_p("[NOISE]", 0.9));
    }

    #[test]
    fn parakeet_youtube_outros_long_form_still_excluded() {
        // Long unambiguous YouTube canned phrases are always-rejected on
        // Parakeet too.
        assert!(!include_p("Thanks for watching.", 0.9));
        assert!(!include_p("Subtitles by the Amara.org community", 0.9));
    }

    #[test]
    fn single_word_repetition_threshold_six() {
        // Threshold raised from 3 to 6 — short stutters pass.
        assert!(!has_excessive_repetition("the the the the"));
        assert!(!has_excessive_repetition("hello hello hello"));
        assert!(has_excessive_repetition("the the the the the the"));
    }

    #[test]
    fn phrase_repetition_threshold_six() {
        assert!(!has_excessive_repetition("thank you thank you thank you"));
        assert!(has_excessive_repetition(
            "thank you thank you thank you thank you thank you thank you"
        ));
    }

    #[test]
    fn normal_text_no_repetition() {
        assert!(!has_excessive_repetition("Hello, how are you today?"));
        assert!(!has_excessive_repetition("The meeting went well"));
    }

    #[test]
    fn short_text_not_flagged() {
        assert!(!has_excessive_repetition("the the"));
        assert!(!has_excessive_repetition("ok"));
    }

    #[test]
    fn real_speech_repetition_high_confidence_passes() {
        // "no no no that's not what I meant" is real speech — must not be
        // dropped at high confidence.
        assert!(include_p("no no no that's not what I meant", 0.85));
    }

    #[test]
    fn long_repetition_low_confidence_filtered() {
        // 6+ reps at confidence < 0.7 still drops — stuck-loop defense.
        assert!(!include_p("the the the the the the the the the", 0.5));
    }

    #[test]
    fn long_repetition_high_confidence_passes() {
        // 6+ reps at confidence ≥ 0.7 passes — extreme but treated as real.
        assert!(include_p("the the the the the the the the the", 0.85));
    }

    #[test]
    fn conversational_fillers_not_filtered_at_high_confidence() {
        assert!(include_p("So", 0.7));
        assert!(include_p("Okay.", 0.7));
        assert!(include_p("Uh", 0.7));
        assert!(include_p("Um", 0.7));
        assert!(include_p("Hmm.", 0.7));
        assert!(include_p("You know", 0.7));
        assert!(include_p("I mean", 0.7));
        assert!(include_p("Yeah", 0.7));
        assert!(include_p("Right.", 0.7));
    }

    #[test]
    fn filler_patterns_filtered_at_marginal_confidence() {
        assert!(!include_p("Yeah", 0.5));
        assert!(!include_p("yeah.", 0.5));
        assert!(!include_p("Okay.", 0.5));
        assert!(!include_p("Um", 0.5));
        assert!(!include_p("So", 0.5));
        assert!(!include_p("Right.", 0.5));
    }

    #[test]
    fn punctuation_joined_short_repetition_passes_high_conf() {
        // 3 reps at high confidence used to be filtered; now they pass.
        assert!(include_p("Yeah.Yeah.Yeah.", 0.9));
        assert!(include_p("No.No.No.No.", 0.9));
    }

    #[test]
    fn punctuation_joined_long_repetition_low_conf_filtered() {
        assert!(!include_p("Yeah.Yeah.Yeah.Yeah.Yeah.Yeah.Yeah.", 0.5));
    }

    #[test]
    fn normalize_spacing_basic() {
        assert_eq!(normalize_spacing("Hello.World"), "Hello. World");
        assert_eq!(
            normalize_spacing("First sentence.Then second"),
            "First sentence. Then second"
        );
    }

    #[test]
    fn normalize_spacing_multiple_punctuation() {
        assert_eq!(
            normalize_spacing("What?Really!Yes.OK"),
            "What? Really! Yes. OK"
        );
    }

    #[test]
    fn normalize_spacing_already_correct() {
        assert_eq!(normalize_spacing("Hello. World"), "Hello. World");
        assert_eq!(normalize_spacing("Normal text here"), "Normal text here");
    }

    #[test]
    fn normalize_spacing_preserves_abbreviations() {
        assert_eq!(normalize_spacing("e.g. something"), "e.g. something");
        assert_eq!(normalize_spacing("i.e. this"), "i.e. this");
        assert_eq!(normalize_spacing("3.5 million"), "3.5 million");
    }

    #[test]
    fn normalize_spacing_empty() {
        assert_eq!(normalize_spacing(""), "");
    }

    #[test]
    fn normalize_spacing_comma_before_uppercase() {
        assert_eq!(normalize_spacing("first,Second"), "first, Second");
        assert_eq!(normalize_spacing("ok,let me"), "ok,let me");
    }

    #[test]
    fn sanitize_text_maps_unicode_punctuation() {
        assert_eq!(
            sanitize_text("He said \u{201C}hello\u{201D}"),
            "He said \"hello\""
        );
        assert_eq!(sanitize_text("it\u{2019}s fine"), "it's fine");
        assert_eq!(sanitize_text("wait\u{2014}what"), "wait--what");
        assert_eq!(sanitize_text("1\u{2013}2"), "1-2");
        assert_eq!(sanitize_text("hmm\u{2026}"), "hmm...");
    }

    #[test]
    fn sanitize_text_strips_invisible_formatting() {
        // Zero-width space, ZWNJ, ZWJ, BOM are common decoder noise.
        assert_eq!(sanitize_text("text\u{200B}here"), "texthere");
        assert_eq!(sanitize_text("a\u{FEFF}b"), "ab");
        // Control chars (NUL, BEL) are dropped.
        assert_eq!(sanitize_text("a\u{0007}b"), "ab");
    }

    #[test]
    fn sanitize_text_preserves_printable_unicode() {
        // Accented Latin (French/Spanish/etc.).
        assert_eq!(sanitize_text("Café naïve jalapeño"), "Café naïve jalapeño");
        // Greek.
        assert_eq!(sanitize_text("Γειά σου"), "Γειά σου");
        // Cyrillic (Ukrainian).
        assert_eq!(sanitize_text("Привіт"), "Привіт");
        // Symbol punctuation (musical note) survives — not a control char.
        assert_eq!(
            sanitize_text("Hello \u{266A} world"),
            "Hello \u{266A} world"
        );
    }

    #[test]
    fn sanitize_text_collapses_double_spaces() {
        assert_eq!(sanitize_text("hello  world"), "hello world");
        assert_eq!(sanitize_text("a   b   c"), "a b c");
    }

    #[test]
    fn sanitize_text_empty() {
        assert_eq!(sanitize_text(""), "");
    }

    #[test]
    fn sanitize_text_ascii_passthrough() {
        assert_eq!(sanitize_text("Normal text here."), "Normal text here.");
    }

    #[test]
    fn punctuation_only_segments_excluded() {
        assert!(!include_p("...", 0.9));
        assert!(!include_p("---", 0.9));
        assert!(!include_p(",", 0.9));
        assert!(!include_p("\u{266A}", 0.9));
    }

    #[test]
    fn text_with_punctuation_still_included() {
        assert!(include_p("Hello, world.", 0.9));
        assert!(include_p("Wait, what?", 0.9));
    }

    // --- 16 kHz input ---

    #[cfg(feature = "parakeet")]
    #[test]
    fn sixteen_khz_input_skips_the_resampler() {
        let mut resampler = None;
        let mono = vec![0.25_f32; 1600];
        assert_eq!(to_16k(mono.clone(), 16_000, &mut resampler).unwrap(), mono);
        assert!(resampler.is_none(), "no resampler built for 16 kHz");
    }

    #[cfg(feature = "parakeet")]
    #[test]
    fn resampler_is_kept_per_rate_and_rebuilt_on_change() {
        let mut resampler = None;
        let out = to_16k(vec![0.0; 4800], 48_000, &mut resampler).unwrap();
        assert_eq!(out.len(), 1600);
        assert_eq!(resampler.as_ref().unwrap().rates(), (48_000, 16_000));
        to_16k(vec![0.0; 4410], 44_100, &mut resampler).unwrap();
        assert_eq!(resampler.as_ref().unwrap().rates(), (44_100, 16_000));
        // A 16 kHz file in between leaves the cached resampler alone.
        to_16k(vec![0.0; 160], 16_000, &mut resampler).unwrap();
        assert_eq!(resampler.as_ref().unwrap().rates(), (44_100, 16_000));
    }

    #[cfg(feature = "parakeet")]
    #[test]
    fn reads_a_16khz_mono_wav_unchanged() {
        let path =
            std::env::temp_dir().join(format!("yapstack-read-wav-16k-{}.wav", std::process::id()));
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for v in [0_i16, i16::MAX, -i16::MAX, 1234] {
            writer.write_sample(v).unwrap();
        }
        writer.finalize().unwrap();
        let mut resampler = None;
        let samples = read_wav_as_mono_16k(&path, &mut resampler).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(samples, vec![0.0, 1.0, -1.0, 1234.0 / i16::MAX as f32]);
        assert!(resampler.is_none());
    }
}
