//! Shared hallucination detection for transcription output.
//!
//! Two-tier pattern system:
//!
//! - **Always-reject**: unambiguous YouTube-style canned outros
//!   ("thanks for watching", "subtitles by the amara.org community"). They are
//!   never real speech.
//! - **Marginal-reject**: short fillers and polite phrases ("yeah", "thank
//!   you", "bye"). Real speech at high confidence, likely artifacts at low
//!   confidence, so callers only drop them below a confidence threshold.
//!
//! All matching is case-insensitive with trailing punctuation stripped.

/// Strip trailing punctuation so "Thank you!" matches "thank you".
pub fn strip_trailing_punctuation(text: &str) -> &str {
    text.trim_end_matches(['.', '!', '?', ',', ';', ':'])
}

/// Always-reject list. Only the unambiguous YouTube-style canned outros;
/// single-word and short polite entries live in the marginal tier so
/// legitimate end-of-turn "thank you" / "bye" survive.
const ALWAYS_REJECT: &[&str] = &[
    "thanks for watching",
    "thanks for listening",
    "thank you for watching",
    "thank you for watching please subscribe",
    "thanks for watching please subscribe",
    "like and subscribe",
    "please subscribe",
    "see you next time",
    "subtitles by the amara org community",
    "subtitles by the amara.org community",
    "...",
];

/// Marginal-reject list. Real speech at high confidence, likely artifact at
/// low confidence.
const MARGINAL_REJECT: &[&str] = &[
    "yeah",
    "yes",
    "no",
    "okay",
    "oh",
    "hmm",
    "uh",
    "um",
    "so",
    "right",
    // Real speech in real conversations; only dropped at low confidence.
    "you",
    "bye",
    "the end",
    "subscribe",
    "thank you",
];

/// Returns `true` if the text matches an always-reject hallucination pattern.
/// Strips trailing punctuation and compares case-insensitively.
pub fn is_always_reject(text: &str) -> bool {
    let trimmed = strip_trailing_punctuation(text.trim());
    let lower = trimmed.to_lowercase();
    ALWAYS_REJECT.contains(&lower.as_str())
}

/// Returns `true` if the text matches a marginal-reject hallucination pattern.
/// Strips trailing punctuation and compares case-insensitively.
pub fn is_marginal_reject(text: &str) -> bool {
    let trimmed = strip_trailing_punctuation(text.trim());
    let lower = trimmed.to_lowercase();
    MARGINAL_REJECT.contains(&lower.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- strip_trailing_punctuation ---

    #[test]
    fn strip_period() {
        assert_eq!(strip_trailing_punctuation("hello."), "hello");
    }

    #[test]
    fn strip_exclamation() {
        assert_eq!(strip_trailing_punctuation("hello!"), "hello");
    }

    #[test]
    fn strip_question() {
        assert_eq!(strip_trailing_punctuation("hello?"), "hello");
    }

    #[test]
    fn strip_multiple() {
        assert_eq!(strip_trailing_punctuation("hello!?"), "hello");
    }

    #[test]
    fn strip_none() {
        assert_eq!(strip_trailing_punctuation("hello"), "hello");
    }

    #[test]
    fn strip_preserves_internal_punctuation() {
        assert_eq!(
            strip_trailing_punctuation("e.g. something"),
            "e.g. something"
        );
    }

    // --- Short polite entries are NOT always-rejected ---

    #[test]
    fn parakeet_does_not_always_reject_short_polite_entries() {
        // Real speech at high confidence; the marginal tier handles them.
        assert!(!is_always_reject("thank you"));
        assert!(!is_always_reject("Thank you."));
        assert!(!is_always_reject("Bye"));
        assert!(!is_always_reject("Subscribe"));
        assert!(!is_always_reject("The end"));
        assert!(!is_always_reject("you"));
    }

    #[test]
    fn parakeet_still_always_rejects_youtube_outros() {
        // Long unambiguous YouTube canned phrases are never real speech.
        assert!(is_always_reject("Thanks for watching"));
        assert!(is_always_reject("thank you for watching please subscribe"));
        assert!(is_always_reject("subtitles by the amara.org community"));
    }

    // --- Marginal: dropped only at low confidence ---

    #[test]
    fn marginal_filler_words() {
        assert!(is_marginal_reject("yeah"));
        assert!(is_marginal_reject("Okay."));
        assert!(is_marginal_reject("Right!"));
    }

    #[test]
    fn parakeet_marginal_includes_demoted_whisper_always() {
        // Low-confidence versions still get dropped.
        assert!(is_marginal_reject("thank you"));
        assert!(is_marginal_reject("Bye"));
        assert!(is_marginal_reject("you"));
    }

    #[test]
    fn marginal_normal_text_passes() {
        assert!(!is_marginal_reject("Yeah that sounds good"));
    }

    #[test]
    fn always_reject_normal_text_passes() {
        assert!(!is_always_reject(
            "Thank you for your help with the project"
        ));
        assert!(!is_always_reject("Hello, how are you?"));
    }
}
