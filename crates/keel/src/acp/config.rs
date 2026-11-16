//! An agent's session configuration options and modes → Keel's catalog and
//! modes.
//!
//! ACP agents describe their settings per session: select options with a
//! category, such as `model` or `thought_level`, and optionally the older
//! session modes. The model option becomes the catalog's models, the
//! thought level option their efforts, and the mode option (or the session
//! modes) [`session::Modes`].

use agent_client_protocol::v1::schema as wire;

use crate::catalog::{self, FastMode};
use crate::session;

/// The id Keel uses for an agent's only model when it offers no choice.
pub(super) const DEFAULT_MODEL: &str = "default";

/// One choice of a select option.
pub(super) struct Choice {
    pub(super) value: String,
    pub(super) name: String,
    pub(super) description: Option<String>,
}

/// The select option of a category, its current value and its choices.
pub(super) fn select<'a>(
    options: &'a [wire::SessionConfigOption],
    category: &wire::SessionConfigOptionCategory,
) -> Option<(&'a wire::SessionConfigOption, String, Vec<Choice>)> {
    options.iter().find_map(|option| {
        if option.category.as_ref() != Some(category) {
            return None;
        }
        let wire::SessionConfigKind::Select(select) = &option.kind else {
            return None;
        };
        let choice = |choice: &wire::SessionConfigSelectOption| Choice {
            value: choice.value.0.to_string(),
            name: choice.name.clone(),
            description: choice.description.clone(),
        };
        let choices = match &select.options {
            wire::SessionConfigSelectOptions::Ungrouped(choices) => {
                choices.iter().map(choice).collect()
            }
            wire::SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|group| group.options.iter().map(choice))
                .collect(),
            _ => Vec::new(),
        };
        Some((option, select.current_value.0.to_string(), choices))
    })
}

/// The current value of a category's select option.
pub(super) fn current(
    options: &[wire::SessionConfigOption],
    category: &wire::SessionConfigOptionCategory,
) -> Option<String> {
    select(options, category).map(|(_, current, _)| current)
}

/// Models from the agent's model option, each with the thought levels of
/// the session. Without a model option, one model, [`DEFAULT_MODEL`],
/// stands for whatever the agent uses.
pub(super) fn models(options: &[wire::SessionConfigOption]) -> Vec<catalog::Model> {
    let (efforts, default_effort) =
        match select(options, &wire::SessionConfigOptionCategory::ThoughtLevel) {
            Some((_, current, choices)) => (
                choices
                    .into_iter()
                    .map(|choice| catalog::Effort {
                        id: choice.value,
                        description: choice.description.unwrap_or(choice.name),
                    })
                    .collect(),
                Some(current),
            ),
            None => (Vec::new(), None),
        };
    let model = |id: String, label: String, description: String, is_default: bool| catalog::Model {
        id,
        label,
        description,
        is_default,
        efforts: efforts.clone(),
        default_effort: default_effort.clone(),
        fast_mode: FastMode::Unavailable,
    };
    match select(options, &wire::SessionConfigOptionCategory::Model) {
        Some((_, current, choices)) => choices
            .into_iter()
            .map(|choice| {
                let is_default = choice.value == current;
                model(
                    choice.value,
                    choice.name,
                    choice.description.unwrap_or_default(),
                    is_default,
                )
            })
            .collect(),
        None => vec![model(
            DEFAULT_MODEL.to_owned(),
            "Default".to_owned(),
            "The agent's own choice".to_owned(),
            true,
        )],
    }
}

/// The agent's modes: its mode option when it has one, otherwise its
/// session modes.
pub(super) fn modes(
    options: &[wire::SessionConfigOption],
    legacy: Option<&wire::SessionModeState>,
) -> Option<session::Modes> {
    if let Some((_, current, choices)) = select(options, &wire::SessionConfigOptionCategory::Mode) {
        return Some(session::Modes {
            current,
            available: choices
                .into_iter()
                .map(|choice| session::Mode {
                    id: choice.value,
                    name: choice.name,
                    description: choice.description,
                })
                .collect(),
        });
    }
    let legacy = legacy?;
    Some(session::Modes {
        current: legacy.current_mode_id.0.to_string(),
        available: legacy
            .available_modes
            .iter()
            .map(|mode| session::Mode {
                id: mode.id.0.to_string(),
                name: mode.name.clone(),
                description: mode.description.clone(),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn options() -> Vec<wire::SessionConfigOption> {
        serde_json::from_value(json!([
            {
                "id": "model", "name": "Model", "category": "model", "type": "select",
                "currentValue": "fast",
                "options": [
                    { "value": "fast", "name": "Fast", "description": "Quick answers" },
                    { "value": "smart", "name": "Smart" }
                ]
            },
            {
                "id": "effort", "name": "Thinking", "category": "thought_level", "type": "select",
                "currentValue": "low",
                "options": [
                    { "group": "levels", "name": "Levels", "options": [
                        { "value": "low", "name": "Low" },
                        { "value": "high", "name": "High" }
                    ]}
                ]
            },
            {
                "id": "mode", "name": "Mode", "category": "mode", "type": "select",
                "currentValue": "ask",
                "options": [ { "value": "ask", "name": "Ask" }, { "value": "code", "name": "Code" } ]
            }
        ]))
        .unwrap()
    }

    #[test]
    fn model_and_thought_level_options_become_the_catalog() {
        let models = models(&options());
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "fast");
        assert!(models[0].is_default);
        assert!(!models[1].is_default);
        assert_eq!(models[0].description, "Quick answers");
        let efforts: Vec<&str> = models[1].efforts.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(efforts, ["low", "high"]);
        assert_eq!(models[1].default_effort.as_deref(), Some("low"));
    }

    #[test]
    fn without_options_there_is_one_default_model() {
        let models = models(&[]);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, DEFAULT_MODEL);
        assert!(models[0].efforts.is_empty());
    }

    #[test]
    fn modes_prefer_the_mode_option() {
        let modes = modes(&options(), None).unwrap();
        assert_eq!(modes.current, "ask");
        assert_eq!(modes.available.len(), 2);
        let legacy: wire::SessionModeState = serde_json::from_value(json!({
            "currentModeId": "plan",
            "availableModes": [ { "id": "plan", "name": "Plan" } ]
        }))
        .unwrap();
        assert_eq!(super::modes(&[], Some(&legacy)).unwrap().current, "plan");
        assert_eq!(super::modes(&[], None), None);
    }
}
