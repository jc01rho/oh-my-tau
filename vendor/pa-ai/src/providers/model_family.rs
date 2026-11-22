//! Model-family classification shared by the provider request builders (Claude adaptive thinking,
//! Gemini thinking budgets, Gemma detection). The parser lives in `pa-types` so the service-tier
//! predicate there uses the same classification.

pub use pa_types::ai::model_family::{classify, ClaudeTier, GeminiVariant, ModelFamily, Revision};
