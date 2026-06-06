//! Builds ACP session config options for model and reasoning-depth pickers.
//!
//! ACP exposes per-session settings through [`SessionConfigOption`]s carried in
//! the `session/new` response and refreshed by the `session/set_config_option`
//! reverse request. Each option renders as an independent dropdown in the
//! client (Zed), so the bridge surfaces two: a **model** selector
//! ([`CONFIG_MODEL_ID`], category [`SessionConfigOptionCategory::Model`]) and a
//! **reasoning depth** selector ([`CONFIG_EFFORT_ID`], category
//! [`SessionConfigOptionCategory::ThoughtLevel`]).
//!
//! This mirrors the TUI's `/model` picker and `Ctrl+T` depth cycle, and follows
//! the same shape opencode uses over ACP. The model list comes from
//! `engine.list_models()`; the depth list comes from the active model's
//! `supported_efforts`, so the offered depths always match what the model
//! accepts.

use agent_client_protocol::schema::{
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
};
use zhive_proto::domain::ThinkingEffort;
use zhive_proto::rpc::ModelDescriptor;

/// Config id for the model selector (stable wire string).
pub(crate) const CONFIG_MODEL_ID: &str = "model";
/// Config id for the reasoning-depth selector (stable wire string).
pub(crate) const CONFIG_EFFORT_ID: &str = "effort";

/// Builds the session config options for the model and reasoning-depth pickers.
///
/// Returns up to two [`SessionConfigOption`]s: a model selector (omitted when
/// `models` is empty, e.g. no catalog) and a reasoning-depth selector (omitted
/// when the active model declares no depth levels). `session_effort` is the
/// depth already chosen for the session, used as the selector's current value
/// when the active model still supports it.
///
/// # Examples
///
/// ```
/// use zhive_bridge_acp::config_option::build_config_options;
/// use zhive_proto::domain::ThinkingEffort;
/// use zhive_proto::rpc::ModelDescriptor;
///
/// let mut m = ModelDescriptor::new("claude-opus-4-8".into());
/// m.active = true;
/// m.supported_efforts = vec![ThinkingEffort::Off, ThinkingEffort::High];
/// let opts = build_config_options(&[m], Some(ThinkingEffort::High));
/// assert_eq!(opts.len(), 2, "model + effort selectors");
/// ```
#[must_use]
pub fn build_config_options(
    models: &[ModelDescriptor],
    session_effort: Option<ThinkingEffort>,
) -> Vec<SessionConfigOption> {
    let mut out = Vec::with_capacity(2);

    if let Some(model) = model_option(models) {
        out.push(model);
    }
    if let Some(effort) = effort_option(models, session_effort) {
        out.push(effort);
    }
    out
}

/// Returns the active model descriptor, falling back to the first one.
fn active_model(models: &[ModelDescriptor]) -> Option<&ModelDescriptor> {
    models.iter().find(|m| m.active).or_else(|| models.first())
}

/// Builds the model selector option, or `None` when no models are known.
fn model_option(models: &[ModelDescriptor]) -> Option<SessionConfigOption> {
    let current = active_model(models)?;
    let options: Vec<SessionConfigSelectOption> = models
        .iter()
        .map(|m| {
            let name = m.display_name.clone().unwrap_or_else(|| m.id.clone());
            SessionConfigSelectOption::new(m.id.clone(), name)
        })
        .collect();
    Some(
        SessionConfigOption::select(CONFIG_MODEL_ID, "Model", current.id.clone(), options)
            .category(SessionConfigOptionCategory::Model),
    )
}

/// Builds the reasoning-depth selector, or `None` when the model has no depths.
///
/// The current value is `session_effort` when the active model still supports
/// it, otherwise the model's first (Off-first) depth, so the displayed value is
/// always one the model accepts.
fn effort_option(
    models: &[ModelDescriptor],
    session_effort: Option<ThinkingEffort>,
) -> Option<SessionConfigOption> {
    let model = active_model(models)?;
    if model.supported_efforts.is_empty() {
        return None;
    }
    let current = session_effort
        .filter(|e| model.supported_efforts.contains(e))
        .or_else(|| model.supported_efforts.first().copied())
        .unwrap_or(ThinkingEffort::Off);

    let options: Vec<SessionConfigSelectOption> = model
        .supported_efforts
        .iter()
        .map(|e| SessionConfigSelectOption::new(e.label(), title_case(e.label())))
        .collect();

    Some(
        SessionConfigOption::select(CONFIG_EFFORT_ID, "Reasoning", current.label(), options)
            .category(SessionConfigOptionCategory::ThoughtLevel),
    )
}

/// Capitalizes the first ASCII letter for a display label (`off` -> `Off`).
fn title_case(label: &str) -> String {
    let mut chars = label.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, active: bool, efforts: Vec<ThinkingEffort>) -> ModelDescriptor {
        let mut m = ModelDescriptor::new(id.to_owned());
        m.active = active;
        m.supported_efforts = efforts;
        m
    }

    #[test]
    fn empty_catalog_yields_no_options() {
        assert!(build_config_options(&[], None).is_empty());
    }

    #[test]
    fn model_without_efforts_omits_effort_option() {
        let opts = build_config_options(&[model("gpt-x", true, vec![])], None);
        assert_eq!(opts.len(), 1, "only the model selector");
        assert_eq!(opts[0].id.0.as_ref(), CONFIG_MODEL_ID);
    }

    #[test]
    fn active_model_drives_current_value() {
        let models = vec![
            model("a", false, vec![]),
            model("b", true, vec![ThinkingEffort::Off, ThinkingEffort::High]),
        ];
        let opts = build_config_options(&models, Some(ThinkingEffort::High));
        let model_opt = &opts[0];
        match &model_opt.kind {
            agent_client_protocol::schema::SessionConfigKind::Select(sel) => {
                assert_eq!(sel.current_value.0.as_ref(), "b");
            }
            _ => panic!("expected a select kind"),
        }
    }

    #[test]
    fn unsupported_session_effort_falls_back_to_model_default() {
        // The session picked Max, but the active model only offers Off/Low.
        let models = vec![model(
            "m",
            true,
            vec![ThinkingEffort::Off, ThinkingEffort::Low],
        )];
        let opts = build_config_options(&models, Some(ThinkingEffort::Max));
        let effort = opts.iter().find(|o| o.id.0.as_ref() == CONFIG_EFFORT_ID);
        let effort = effort.expect("effort option present");
        match &effort.kind {
            agent_client_protocol::schema::SessionConfigKind::Select(sel) => {
                assert_eq!(
                    sel.current_value.0.as_ref(),
                    "off",
                    "falls back to the model's first depth"
                );
            }
            _ => panic!("expected a select kind"),
        }
    }

    #[test]
    fn title_case_capitalizes() {
        assert_eq!(title_case("off"), "Off");
        assert_eq!(title_case("xhigh"), "Xhigh");
        assert_eq!(title_case(""), "");
    }
}

// Rust guideline compliant 2026-02-21
