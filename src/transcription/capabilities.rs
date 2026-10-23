//! One capability contract for the API, controls and result metadata.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct BackendCapabilities {
    pub id: &'static str,
    /// Segment timing is supported only by Whisper. Sidecar clip bounds are
    /// synthetic, not word/segment alignment.
    pub timestamps: &'static str,
    pub confidence: bool,
    pub translation: bool,
    pub language_hint: bool,
    pub live_preview: bool,
}

pub const BACKENDS: [BackendCapabilities; 3] = [
    BackendCapabilities {
        id: "whisper",
        timestamps: "segments",
        confidence: true,
        translation: true,
        language_hint: true,
        live_preview: true,
    },
    BackendCapabilities {
        id: "qwen",
        timestamps: "unavailable",
        confidence: false,
        translation: false,
        language_hint: true,
        live_preview: false,
    },
    BackendCapabilities {
        id: "parakeet",
        timestamps: "clip_bounds_only",
        confidence: false,
        translation: false,
        language_hint: false,
        live_preview: false,
    },
];

pub fn for_backend(id: &str) -> Option<BackendCapabilities> {
    BACKENDS.iter().find(|backend| backend.id == id).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sidecars_do_not_claim_alignment_confidence_or_translation() {
        for backend in &BACKENDS[1..] {
            assert_ne!(backend.timestamps, "segments");
            assert!(!backend.confidence && !backend.translation && !backend.live_preview);
        }
        assert!(for_backend("unknown").is_none());
    }
}
