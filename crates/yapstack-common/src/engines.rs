//! Engine capability catalogue.
//!
//! Source of truth for which transcription engines exist, what languages
//! each supports, and which optional features they expose. Used by:
//!
//! - the sidecar to validate `Transcribe` requests against the engine it
//!   was spawned with,
//! - the Tauri command layer to compose engine descriptors with downloaded
//!   model metadata when serving the frontend,
//! - the frontend (indirectly via that command) to drive cascading
//!   the engine block and its supported-languages list.
//!
//! Model lists live in `yapstack-transcription` (their download URLs and
//! sizes don't belong here). The frontend joins the two.

use crate::types::EngineKind;

/// Capabilities of a single transcription engine.
pub struct EngineDescriptor {
    pub kind: EngineKind,
    pub display_name: &'static str,
    /// BCP-47 / ISO-639-1 codes the engine can transcribe. The first entry
    /// is the engine's primary language.
    pub languages: &'static [&'static str],
    /// True when the engine supports an optional speaker-diarization pass
    /// (today: Parakeet via Sortformer).
    pub supports_diarization: bool,
}

impl EngineDescriptor {
    pub fn supports_language(&self, code: &str) -> bool {
        self.languages.iter().any(|l| l.eq_ignore_ascii_case(code))
    }
}

/// All engines Yapstack can dispatch to. Order is the UI presentation order.
pub fn engine_catalogue() -> &'static [EngineDescriptor] {
    &CATALOGUE
}

pub fn descriptor(kind: EngineKind) -> &'static EngineDescriptor {
    engine_catalogue()
        .iter()
        .find(|d| d.kind == kind)
        .expect("every EngineKind variant has a catalogue entry")
}

static CATALOGUE: [EngineDescriptor; 1] = [EngineDescriptor {
    kind: EngineKind::Parakeet,
    display_name: "Parakeet",
    languages: PARAKEET_TDT_V3_LANGUAGES,
    supports_diarization: true,
}];

/// Parakeet TDT-0.6b-v3 supports 25 European languages.
/// Source: nvidia/parakeet-tdt-0.6b-v3 model card on HuggingFace.
/// Refine when the Parakeet backend lands by reading the tokenizer's
/// declared language set.
static PARAKEET_TDT_V3_LANGUAGES: &[&str] = &[
    "en", "bg", "hr", "cs", "da", "nl", "et", "fi", "fr", "de", "el", "hu", "ga", "it", "lv", "lt",
    "mt", "pl", "pt", "ro", "sk", "sl", "es", "sv", "uk",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_has_exactly_one_engine() {
        let catalogue = engine_catalogue();
        assert_eq!(catalogue.len(), 1);
        assert_eq!(catalogue[0].kind, EngineKind::Parakeet);
    }

    #[test]
    fn parakeet_catalogue_matches_25_language_count() {
        let d = descriptor(EngineKind::Parakeet);
        assert_eq!(
            d.languages.len(),
            25,
            "Parakeet TDT v3 publishes 25 languages"
        );
        assert!(d.supports_diarization);
        assert!(d.supports_language("en"));
        assert!(d.supports_language("de"));
        assert!(!d.supports_language("ja"));
    }

    #[test]
    fn descriptor_lookup_is_total() {
        assert_eq!(descriptor(EngineKind::Parakeet).kind, EngineKind::Parakeet);
    }
}
