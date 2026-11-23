//! A resolved-policy view of one model: the complete compat record and thinking config the TS
//! request builders read as `model.compat.*` / `model.thinking.*`.

use serde_json::{Map, Value};

use pa_types::ai::{get_supported_thinking_levels, Model, ModelThinkingLevel};

use super::resolve::{resolve_model_policy, ModelSpec};

#[derive(Debug, Clone, Default)]
pub struct ModelView {
    pub compat: Map<String, Value>,
    pub thinking: Option<Value>,
}

impl ModelView {
    /// The policy `buildModel` bakes for `model` (sparse overrides in `model.compat`).
    #[must_use]
    pub fn of(model: &Model) -> Self {
        Self::with_thinking(model, None)
    }

    /// Like [`Self::of`] with an authored thinking config (TS `spec.thinking`).
    #[must_use]
    pub fn with_thinking(model: &Model, explicit_thinking: Option<Value>) -> Self {
        let spec = ModelSpec {
            provider: model.provider.clone(),
            id: model.id.clone(),
            api: model.api.clone(),
            base_url: model.base_url.clone(),
            provider_type: None,
            reasoning: model.reasoning,
            thinking: explicit_thinking,
            compat: model
                .compat
                .as_ref()
                .map(|c| c.raw.clone().into_iter().collect()),
            lenient: true,
        };
        let has_explicit = spec.thinking.is_some();
        match resolve_model_policy(&spec, None) {
            Ok(policy) => {
                let mut view = Self {
                    compat: policy.compat.unwrap_or_default(),
                    thinking: policy.thinking,
                };
                if !has_explicit {
                    view.apply_materialized_ladder(model);
                }
                view
            }
            Err(_) => Self::default(),
        }
    }

    /// A model materialized from an authored thinking config (a cached row's stored `thinking`)
    /// carries that ladder in its `thinking_level_map`; the effort set the request reads must be
    /// the one the caller clamped against, not a fresh re-derivation. A sparse authored map (a
    /// `models.yml` or extension `thinkingLevelMap` such as `{ max: "max" }`) leaves its absent
    /// levels at their defaults, the same rule [`get_supported_thinking_levels`] clamps by.
    fn apply_materialized_ladder(&mut self, model: &Model) {
        let (Some(map), Some(Value::Object(thinking))) =
            (&model.thinking_level_map, self.thinking.as_mut())
        else {
            return;
        };
        let derived: Vec<&str> = get_supported_thinking_levels(model)
            .into_iter()
            .filter(|l| *l != ModelThinkingLevel::Off)
            .map(ModelThinkingLevel::wire_name)
            .collect();
        let current: Vec<&str> = thinking
            .get("efforts")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if derived == current || derived.is_empty() {
            return;
        }
        thinking.insert(
            "efforts".into(),
            Value::Array(
                derived
                    .iter()
                    .map(|e| Value::String((*e).to_string()))
                    .collect(),
            ),
        );
        let mut effort_map = thinking
            .get("effortMap")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter(|(k, _)| {
                        derived.contains(&k.as_str())
                            && !map.keys().any(|level| level.wire_name() == k.as_str())
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Map<String, Value>>()
            })
            .unwrap_or_default();
        for (level, value) in map {
            if let Some(v) = value {
                if v != level.wire_name() {
                    effort_map.insert(level.wire_name().to_string(), Value::String(v.clone()));
                }
            }
        }
        if effort_map.is_empty() {
            thinking.remove("effortMap");
        } else {
            thinking.insert("effortMap".into(), Value::Object(effort_map));
        }
    }

    #[must_use]
    pub fn flag(&self, key: &str) -> Option<bool> {
        self.compat.get(key).and_then(Value::as_bool)
    }

    /// `compat.key === true`.
    #[must_use]
    pub fn is(&self, key: &str) -> bool {
        self.flag(key) == Some(true)
    }

    /// `compat.key !== false`.
    #[must_use]
    pub fn not_false(&self, key: &str) -> bool {
        self.flag(key) != Some(false)
    }

    #[must_use]
    pub fn compat_str(&self, key: &str) -> Option<&str> {
        self.compat.get(key).and_then(Value::as_str)
    }

    #[must_use]
    pub fn thinking_str(&self, key: &str) -> Option<&str> {
        self.thinking.as_ref()?.get(key)?.as_str()
    }

    #[must_use]
    pub fn thinking_flag(&self, key: &str) -> Option<bool> {
        self.thinking.as_ref()?.get(key)?.as_bool()
    }

    /// The thinking ladder, lowest first.
    #[must_use]
    pub fn efforts(&self) -> Vec<&str> {
        self.thinking
            .as_ref()
            .and_then(|t| t.get("efforts"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }

    /// `thinking.effortMap[effort]`.
    #[must_use]
    pub fn effort_map(&self, effort: &str) -> Option<&str> {
        self.thinking
            .as_ref()?
            .get("effortMap")?
            .get(effort)?
            .as_str()
    }

    /// `thinking.effortRouting[effort ?? "off"]`.
    #[must_use]
    pub fn effort_routing(&self, effort: Option<&str>) -> Option<&str> {
        self.thinking
            .as_ref()?
            .get("effortRouting")?
            .get(effort.unwrap_or("off"))?
            .as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_types::ai::{thinking_level_map, ModelCost, ModelInput};
    use pa_types::JsNumber;

    fn relay_model(map: &[(ModelThinkingLevel, Option<&str>)]) -> Model {
        Model {
            id: "ollama-deepseek-v4.1-flash".into(),
            name: "DeepSeek V4.1 Flash".into(),
            api: "openai-responses".into(),
            provider: "omomeow-opus".into(),
            base_url: "http://127.0.0.1:9/v1".into(),
            reasoning: true,
            thinking_level_map: Some(thinking_level_map(map)),
            input: vec![ModelInput::Text],
            cost: ModelCost {
                input: JsNumber::from(0.0),
                output: JsNumber::from(0.0),
                cache_read: JsNumber::from(0.0),
                cache_write: JsNumber::from(0.0),
            },
            context_window: 256_000,
            max_tokens: 64_000,
            max_tokens_explicit: true,
            omit_max_output_tokens: None,
            request_model_id: None,
            featured: None,
            headers: None,
            compat: None,
            tokenizer: None,
        }
    }

    // The OmOMeow gateway registers `{ max: "max" }` to add max on top of the default levels; a
    // lane asking for `high` was refused with "Supported efforts: max" (omt 0.13.1 canary).
    #[test]
    fn sparse_map_keeps_absent_levels_at_their_defaults() {
        let model = relay_model(&[(ModelThinkingLevel::Max, Some("max"))]);
        let view = ModelView::of(&model);
        let efforts = view.efforts();
        assert!(efforts.contains(&"high"), "efforts: {efforts:?}");
        assert!(efforts.contains(&"max"), "efforts: {efforts:?}");
        assert!(!efforts.contains(&"xhigh"), "efforts: {efforts:?}");
    }

    #[test]
    fn null_entries_remove_levels_and_absent_high_stays() {
        let model = relay_model(&[
            (ModelThinkingLevel::Off, None),
            (ModelThinkingLevel::Minimal, None),
            (ModelThinkingLevel::Low, None),
            (ModelThinkingLevel::Medium, None),
            (ModelThinkingLevel::Max, Some("max")),
        ]);
        assert_eq!(ModelView::of(&model).efforts(), vec!["high", "max"]);
    }
}
