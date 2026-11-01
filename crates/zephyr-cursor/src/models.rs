//! The model catalog this package replaces rpi's with.
//!
//! Every entry is a slug the agent service answers, and the table below is the only place that
//! decides anything about them: `modes_of` reads it, so the catalog's `reasoning` flag and the
//! request's `ModelDetails` can never disagree. Context windows are the engine's documented
//! ceilings, not per-model facts: the server rejects an over-long request, and the numbers only
//! steer rpi's compaction.
use rpi_ai::{Api, Model};
use std::sync::OnceLock;

/// Default model when the account names none; Cursor serves it on every plan.
pub const DEFAULT_MODEL: &str = "composer-2.5";

/// Engine ceilings, shared by the catalog entries.
const CONTEXT_WINDOW: u64 = 200_000;
const MAX_TOKENS: u64 = 64_000;

/// `(id, display name, thinking)` — the slugs Cursor's backend answers, the same list the reference
/// clients send (`jcode`'s `AVAILABLE_MODELS`). `thinking` says whether the slug reasons at all;
/// Cursor takes that from `ModelDetails`, not from the id, so this is what the request says too.
const SLUGS: &[(&str, &str, bool)] = &[
    ("composer-2.5", "Composer 2.5", true),
    ("composer-2-fast", "Composer 2 (fast)", false),
    ("composer-2", "Composer 2", true),
    ("grok-4.7", "Grok 4.7", true),
    ("grok-4.6", "Grok 4.6", true),
    ("gpt-5.4-high", "GPT-5.4 (high)", true),
    ("gpt-5.4-medium", "GPT-5.4 (medium)", true),
    ("gpt-5.4-low", "GPT-5.4 (low)", false),
    ("gpt-5", "GPT-5", true),
    ("sonnet-4.6", "Claude Sonnet 4.6", false),
    ("sonnet-4.6-thinking", "Claude Sonnet 4.6 (thinking)", true),
    ("opus-4.6", "Claude Opus 4.6", true),
    ("gemini-3.1-pro", "Gemini 3.1 Pro", true),
];

/// The catalog, built once: every entry is the same protocol, so they differ only in name and
/// reasoning support.
pub fn catalog() -> &'static [Model] {
    static CATALOG: OnceLock<Vec<Model>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        SLUGS
            .iter()
            .map(|(id, name, _)| {
                let mut model = Model::new(
                    *id,
                    *name,
                    Api::OpenaiCompletions,
                    crate::ID,
                    crate::BASE_URL,
                );
                model.reasoning = modes_of(id).0;
                model.context_window = CONTEXT_WINDOW;
                model.max_tokens = MAX_TOKENS;
                model
            })
            .collect()
    })
}

/// `(thinking, fast)` for a slug: `thinking` from the table, `fast` from the id's suffix (the one
/// mode Cursor has no separate base slug for). An unknown slug keeps thinking on, because guessing
/// "no" would strip a reasoning model's blocks.
pub fn modes_of(model: &str) -> (bool, bool) {
    let thinking = SLUGS
        .iter()
        .find(|(id, _, _)| *id == model)
        .map(|(_, _, thinking)| *thinking)
        .unwrap_or(true);
    (thinking, model.ends_with("-fast"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_is_served_by_this_provider_and_names_the_default_model() {
        let catalog = catalog();
        assert!(catalog.iter().any(|m| m.id == DEFAULT_MODEL));
        assert!(catalog.iter().all(|m| m.provider == crate::ID));
        assert!(catalog.iter().all(|m| m.api == Api::OpenaiCompletions));
        assert!(catalog
            .iter()
            .all(|m| m.context_window > 0 && m.max_tokens > 0));
    }

    #[test]
    fn the_catalog_and_the_request_modes_come_from_one_table() {
        let mut ids: Vec<&str> = catalog().iter().map(|m| m.id.as_str()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "a slug is listed twice");
        for (id, _, thinking) in SLUGS {
            let modes = modes_of(id);
            assert_eq!(modes.0, *thinking, "{id}");
            assert_eq!(modes.1, id.ends_with("-fast"), "{id}");
            let model = catalog().iter().find(|m| m.id == *id).expect("listed");
            assert_eq!(model.reasoning, *thinking, "{id}");
        }
        assert_eq!(
            modes_of("something-new"),
            (true, false),
            "an unknown slug keeps thinking"
        );
    }
}
