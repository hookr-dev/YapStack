//! Engine abstraction layer for the transcription sidecar.
//!
//! Each backend (Whisper, Parakeet) implements [`TranscriptionBackend`].
//! The sidecar is spawned with `--engine whisper|parakeet` and constructs
//! the matching backend; the IPC dispatch loop in `main.rs` is engine-agnostic.

#[cfg(any(feature = "whisper", feature = "parakeet"))]
use std::path::Path;

#[cfg(any(feature = "whisper", feature = "parakeet"))]
use yapstack_common::types::TranscriptSegment;

#[cfg(feature = "whisper")]
pub mod whisper;

#[cfg(feature = "parakeet")]
pub mod ort_placement;
#[cfg(feature = "parakeet")]
pub mod parakeet;

/// Per-request transcription options. Engines ignore options they don't
/// support (e.g. Parakeet ignores `initial_prompt`; Whisper ignores
/// `diarization`).
#[cfg(any(feature = "whisper", feature = "parakeet"))]
#[derive(Debug, Clone, Copy)]
pub struct TranscribeOpts<'a> {
    pub language: Option<&'a str>,
    pub initial_prompt: Option<&'a str>,
    pub single_segment: Option<bool>,
    /// Honored by Parakeet via Sortformer; ignored by Whisper.
    #[allow(dead_code)]
    pub diarization: bool,
}

#[cfg(any(feature = "whisper", feature = "parakeet"))]
#[derive(Debug, Clone)]
pub struct TranscriptionOutput {
    pub text: String,
    pub segments: Vec<TranscriptSegment>,
    pub duration_ms: u64,
}

/// Read a WAV file, downmix to mono, and resample to 16 kHz. Both Whisper
/// and Parakeet require this exact input shape.
#[cfg(any(feature = "whisper", feature = "parakeet"))]
pub(crate) fn read_wav_as_mono_16k(audio_path: &Path) -> Result<Vec<f32>, String> {
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
    let mono = yapstack_common::audio::deinterleave_to_mono(&raw, spec.channels);
    let resampled = yapstack_common::audio::resample(&mono, spec.sample_rate, 16_000)
        .map_err(|e| format!("resample to 16kHz failed: {e}"))?;
    Ok(resampled.into_owned())
}

/// Information about the model that a backend has loaded, exposed back
/// to the desktop layer through `SidecarResponse::ModelLoaded` so the
/// frontend can surface "Parakeet · WebGPU" badges without re-deriving
/// from frontend state. `accel` reflects the *resolved* execution
/// provider including any runtime fallback (e.g. `"cpu"` when WebGPU
/// init fails and the load chain falls back).
#[cfg(any(feature = "whisper", feature = "parakeet"))]
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
/// configuration (Whisper VAD model, Parakeet decoder cache, etc.) is
/// passed to the backend's constructor — not through this trait.
#[cfg(any(feature = "whisper", feature = "parakeet"))]
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
// These helpers were originally Whisper-specific. They are mild enough
// that running Parakeet output through them is harmless (Parakeet rarely
// produces YouTube-artifact hallucinations or unicode quote marks, but
// the filters are no-ops on clean text).

/// Insert a space after sentence-ending punctuation (`.` `!` `?`) or commas
/// when immediately followed by an uppercase letter. Fixes joined sentences
/// like "First sentence.Then second" while preserving "e.g." abbreviations
/// (which are followed by lowercase).
#[cfg(any(feature = "whisper", feature = "parakeet", test))]
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
/// preserved verbatim — Parakeet TDT v3 supports 25 European languages and
/// Whisper supports 99, including non-ASCII scripts.
#[cfg(any(feature = "whisper", feature = "parakeet", test))]
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
#[cfg(any(feature = "whisper", feature = "parakeet", test))]
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
/// high-confidence stutters survive on both engines.
#[cfg(any(feature = "whisper", feature = "parakeet", test))]
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
/// `engine` selects the per-engine hallucination tier (Whisper keeps the
/// aggressive always-reject list; Parakeet uses a softer one). The repetition
/// gate is shared but only fires at confidence < 0.7 so legitimate
/// high-confidence stutters survive on both engines.
#[cfg(any(feature = "whisper", feature = "parakeet", test))]
pub(crate) fn should_include_segment(
    text: &str,
    confidence: f32,
    engine: yapstack_common::types::EngineKind,
) -> bool {
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

    if yapstack_common::hallucination::is_always_reject(trimmed, engine) {
        return false;
    }

    if confidence < 0.6 && yapstack_common::hallucination::is_marginal_reject(trimmed, engine) {
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use yapstack_common::types::EngineKind;

    // Convenience: most pre-existing tests asserted Whisper behavior; keep
    // them readable by defaulting to Whisper.
    fn include_w(text: &str, confidence: f32) -> bool {
        should_include_segment(text, confidence, EngineKind::Whisper)
    }

    fn include_p(text: &str, confidence: f32) -> bool {
        should_include_segment(text, confidence, EngineKind::Parakeet)
    }

    #[test]
    fn normal_text_high_confidence_included() {
        assert!(include_w("Hello, how are you?", 0.9));
        assert!(include_p("Hello, how are you?", 0.9));
    }

    #[test]
    fn whisper_thank_you_excluded_all_confidences() {
        assert!(!include_w("Thank you.", 0.3));
        assert!(!include_w("Thank you.", 0.8));
        assert!(!include_w("Thank you!", 0.95));
        assert!(!include_w("Thank you?", 0.9));
        assert!(!include_w("Thank you", 0.99));
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
        assert!(!include_w("", 0.9));
        assert!(!include_w("   ", 0.9));
    }

    #[test]
    fn blank_audio_token_excluded() {
        assert!(!include_w("[BLANK_AUDIO]", 0.9));
    }

    #[test]
    fn whisper_you_always_excluded() {
        assert!(!include_w("you", 0.5));
        assert!(!include_w("you", 0.9));
    }

    #[test]
    fn parakeet_you_marginal_only() {
        // Demoted: high-confidence "you" passes on Parakeet.
        assert!(include_p("you", 0.9));
        assert!(!include_p("you", 0.5));
    }

    #[test]
    fn longer_sentence_with_you_included() {
        assert!(include_w("You should try this", 0.7));
        assert!(include_p("You should try this", 0.7));
    }

    #[test]
    fn below_confidence_threshold_excluded() {
        assert!(!include_w("Some random text", 0.35));
        assert!(!include_p("Some random text", 0.35));
    }

    #[test]
    fn whisper_hallucination_at_boundary_confidence() {
        assert!(!include_w("Thank you.", 0.6));
        assert!(!include_w("Thank you.", 0.59));
        assert!(include_w("Yeah", 0.6));
        assert!(!include_w("Yeah", 0.59));
    }

    #[test]
    fn special_tokens_excluded() {
        assert!(!include_w("[MUSIC]", 0.9));
        assert!(!include_w("[NOISE]", 0.9));
    }

    #[test]
    fn whisper_youtube_outros_case_insensitive() {
        assert!(!include_w("THANK YOU.", 0.9));
        assert!(!include_w("Bye.", 0.9));
        assert!(!include_w("Subscribe.", 0.9));
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
        // dropped at high confidence on either engine.
        assert!(include_w("no no no that's not what I meant", 0.85));
        assert!(include_p("no no no that's not what I meant", 0.85));
    }

    #[test]
    fn long_repetition_low_confidence_filtered() {
        // 6+ reps at confidence < 0.7 still drops — Whisper stuck-loop defense.
        assert!(!include_w("the the the the the the the the the", 0.5));
    }

    #[test]
    fn long_repetition_high_confidence_passes() {
        // 6+ reps at confidence ≥ 0.7 passes — extreme but treated as real.
        assert!(include_w("the the the the the the the the the", 0.85));
    }

    #[test]
    fn conversational_fillers_not_filtered_at_high_confidence() {
        assert!(include_w("So", 0.7));
        assert!(include_w("Okay.", 0.7));
        assert!(include_w("Uh", 0.7));
        assert!(include_w("Um", 0.7));
        assert!(include_w("Hmm.", 0.7));
        assert!(include_w("You know", 0.7));
        assert!(include_w("I mean", 0.7));
        assert!(include_w("Yeah", 0.7));
        assert!(include_w("Right.", 0.7));
    }

    #[test]
    fn filler_patterns_filtered_at_marginal_confidence() {
        assert!(!include_w("Yeah", 0.5));
        assert!(!include_w("yeah.", 0.5));
        assert!(!include_w("Okay.", 0.5));
        assert!(!include_w("Um", 0.5));
        assert!(!include_w("So", 0.5));
        assert!(!include_w("Right.", 0.5));
    }

    #[test]
    fn punctuation_joined_short_repetition_passes_high_conf() {
        // 3 reps at high confidence used to be filtered; now they pass.
        assert!(include_w("Yeah.Yeah.Yeah.", 0.9));
        assert!(include_w("No.No.No.No.", 0.9));
    }

    #[test]
    fn punctuation_joined_long_repetition_low_conf_filtered() {
        assert!(!include_w("Yeah.Yeah.Yeah.Yeah.Yeah.Yeah.Yeah.", 0.5));
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
        // Zero-width space, ZWNJ, ZWJ, BOM are common Whisper noise.
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
        assert!(!include_w("...", 0.9));
        assert!(!include_w("---", 0.9));
        assert!(!include_w(",", 0.9));
        assert!(!include_w("\u{266A}", 0.9));
    }

    #[test]
    fn text_with_punctuation_still_included() {
        assert!(include_w("Hello, world.", 0.9));
        assert!(include_w("Wait, what?", 0.9));
    }
}
