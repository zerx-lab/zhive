//! Maps a per-turn [`ThinkingEffort`] onto provider request controls.
//!
//! The engine reconstructs a provider [`CallOptions`] in [`super::prompt`] and
//! then calls [`apply_reasoning`] to layer the turn's requested reasoning depth
//! on top. Two strategies are used:
//!
//! * **Anthropic models that support the `effort` knob** (Opus 4.5/4.6/4.7/4.8,
//!   Sonnet 4.6) take a provider-specific `provider_options["anthropic"]` block
//!   carrying `thinking` (`adaptive` with `display: "summarized"`, or
//!   `disabled`) and `effort`. Writing these directly is the only way to set
//!   `thinking.display` — without it Opus 4.8/4.7 return empty thinking blocks —
//!   and it sidesteps the SDK's model-capability table (which has not yet
//!   learned `claude-opus-4-8`).
//! * **Everything else** uses the portable [`CallOptions::reasoning`] enum,
//!   which the SDK normalizes per provider (`OpenAI` `reasoning_effort`, Google
//!   thinking budget) and safely ignores on models that do not support it, so
//!   no request ever 400s for an unsupported `effort`.
//!
//! [`ThinkingEffort`]: zhive_proto::domain::ThinkingEffort
//! [`CallOptions`]: llmsdk::language_model::CallOptions

use llmsdk::language_model::{CallOptions, ReasoningEffort};
use zhive_proto::domain::ThinkingEffort;

/// Applies the turn's reasoning depth to a freshly-built [`CallOptions`].
///
/// `reasoning` of `None` is a no-op (the provider default stands). `provider`
/// and `model_id` come from the active [`llmsdk::language_model::LanguageModel`]
/// and select the mapping strategy described in the module docs.
///
/// # Examples
///
/// ```ignore
/// // (crate-internal) apply High reasoning to an Anthropic Opus request:
/// let mut opts = CallOptions::default();
/// apply_reasoning(&mut opts, Some(ThinkingEffort::High), "anthropic", "claude-opus-4-8");
/// assert!(opts.provider_options.is_some());
/// ```
pub(in crate::engine) fn apply_reasoning(
    opts: &mut CallOptions,
    reasoning: Option<ThinkingEffort>,
    provider: &str,
    model_id: &str,
) {
    let Some(level) = reasoning else {
        return;
    };

    if provider == "anthropic" && anthropic_supports_effort(model_id) {
        opts.provider_options = Some(anthropic_provider_options(level, model_id));
    } else if provider == "openai" {
        apply_openai_reasoning(opts, level);
    } else {
        // Other providers (xAI, …): the portable enum, which the SDK gates per
        // model. `Off` is left unset so we never send an explicit "none" a
        // provider might reject.
        if level.is_enabled() {
            opts.reasoning = Some(to_reasoning_effort(level));
        }
    }
}

/// Applies a reasoning level for OpenAI (both the Chat and Responses APIs).
///
/// `Off` is a no-op: it leaves `reasoning_effort` unset so non-reasoning models
/// (e.g. `gpt-4o`) are never sent an effort they reject with a 400 — unlike
/// Anthropic, OpenAI has no "disable thinking" value. For enabled levels the
/// effort is written BOTH ways: the unified `CallOptions.reasoning` (read by the
/// Chat Completions path) and `provider_options["openai"].reasoningEffort` (the
/// only field the Responses path reads), so the effort survives whichever API
/// the SDK is configured to use.
fn apply_openai_reasoning(opts: &mut CallOptions, level: ThinkingEffort) {
    if !level.is_enabled() {
        return;
    }
    opts.reasoning = Some(openai_reasoning_effort(level));

    // Read-modify-write the `openai` bucket rather than replacing it, so this
    // composes with whatever else writes there (notably the `promptCacheKey`
    // from `super::cache`) regardless of call order — only `reasoningEffort` is
    // (re)set; sibling keys survive.
    let mut po = opts.provider_options.take().unwrap_or_default();
    let mut openai = po.remove("openai").unwrap_or_default();
    openai.insert(
        "reasoningEffort".to_owned(),
        serde_json::Value::String(openai_effort_str(level).to_owned()),
    );
    po.insert("openai".to_owned(), openai);
    opts.provider_options = Some(po);
}

/// Maps an enabled level to OpenAI's portable [`ReasoningEffort`] (1:1).
///
/// `Off` is handled by the caller (omitted); `Max` is never offered to OpenAI
/// by [`zhive_proto::domain::ThinkingEffort::cycle_for`], so it falls back to
/// the closest tier defensively.
fn openai_reasoning_effort(level: ThinkingEffort) -> ReasoningEffort {
    match level {
        ThinkingEffort::Minimal => ReasoningEffort::Minimal,
        ThinkingEffort::Low => ReasoningEffort::Low,
        ThinkingEffort::Medium => ReasoningEffort::Medium,
        ThinkingEffort::Xhigh => ReasoningEffort::Xhigh,
        // High, Max, Off, and any future variant.
        _ => ReasoningEffort::High,
    }
}

/// Maps an enabled level to the OpenAI `reasoning_effort` wire string.
fn openai_effort_str(level: ThinkingEffort) -> &'static str {
    match level {
        ThinkingEffort::Minimal => "minimal",
        ThinkingEffort::Low => "low",
        ThinkingEffort::Medium => "medium",
        ThinkingEffort::Xhigh => "xhigh",
        // High, Max, Off, and any future variant.
        _ => "high",
    }
}

/// Returns `true` when `model_id` is an Anthropic model that honors `effort`.
///
/// Mirrors the set of models that support adaptive thinking plus the `effort`
/// parameter. `claude-opus-4-8` is included even though the bundled SDK
/// capability table predates it.
fn anthropic_supports_effort(model_id: &str) -> bool {
    const SUPPORTED: [&str; 5] = [
        "claude-opus-4-5",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-sonnet-4-6",
    ];
    SUPPORTED.iter().any(|m| model_id.contains(m))
}

/// Returns `true` for models that omit thinking text unless `display` is set.
///
/// Opus 4.7 and later return empty thinking placeholders by default, so the
/// engine must request `display: "summarized"` to surface the reasoning. Older
/// models return thinking content already and do not take the field.
fn defaults_to_omitted_thinking(model_id: &str) -> bool {
    model_id.contains("claude-opus-4-7") || model_id.contains("claude-opus-4-8")
}

/// Builds the `provider_options["anthropic"]` block for `level`.
///
/// `Off` disables thinking; any other level enables adaptive thinking with
/// `display: "summarized"` (so the reasoning text is actually returned) plus
/// the mapped `effort`.
fn anthropic_provider_options(
    level: ThinkingEffort,
    model_id: &str,
) -> llmsdk::shared::ProviderOptions {
    let mut anthropic = serde_json::Map::new();
    if level.is_enabled() {
        // `display` is an Opus 4.7+ field: those models omit thinking text by
        // default, so we must opt into `"summarized"` to see it. Earlier
        // models (Opus 4.5/4.6, Sonnet 4.6) return thinking content already and
        // may reject the unknown field, so we leave it off for them.
        let thinking = if defaults_to_omitted_thinking(model_id) {
            serde_json::json!({ "type": "adaptive", "display": "summarized" })
        } else {
            serde_json::json!({ "type": "adaptive" })
        };
        anthropic.insert("thinking".to_owned(), thinking);
        anthropic.insert(
            "effort".to_owned(),
            serde_json::Value::String(effort_str(level).to_owned()),
        );
    } else {
        anthropic.insert(
            "thinking".to_owned(),
            serde_json::json!({ "type": "disabled" }),
        );
    }
    let mut options = llmsdk::shared::ProviderOptions::new();
    options.insert("anthropic".to_owned(), anthropic);
    options
}

/// Maps an enabled `level` to its Anthropic `effort` string, 1:1.
///
/// No model-dependent downgrade: callers only pass a level the model supports
/// (see [`zhive_proto::domain::ThinkingEffort::cycle_for`]), so the effort sent
/// always matches the level the UI displays. `Off` is never passed here (the
/// caller handles it as `thinking: disabled`).
fn effort_str(level: ThinkingEffort) -> &'static str {
    match level {
        ThinkingEffort::Low => "low",
        ThinkingEffort::Medium => "medium",
        ThinkingEffort::Xhigh => "xhigh",
        ThinkingEffort::Max => "max",
        // `High`, `Off`, and any future (non_exhaustive) variant map to `"high"`
        // (`Off` is handled upstream as `thinking: disabled`).
        _ => "high",
    }
}

/// Maps a [`ThinkingEffort`] to the portable [`ReasoningEffort`] enum.
fn to_reasoning_effort(level: ThinkingEffort) -> ReasoningEffort {
    match level {
        ThinkingEffort::Off => ReasoningEffort::None,
        ThinkingEffort::Minimal => ReasoningEffort::Minimal,
        ThinkingEffort::Low => ReasoningEffort::Low,
        ThinkingEffort::Medium => ReasoningEffort::Medium,
        ThinkingEffort::High => ReasoningEffort::High,
        // The portable enum has no `max`; map `Max` to its highest tier
        // alongside `Xhigh`. (Non-Anthropic models never offer `Max` via
        // `cycle_for`, so the `Max` case is defensive only.)
        ThinkingEffort::Xhigh | ThinkingEffort::Max => ReasoningEffort::Xhigh,
        // Unknown future (non_exhaustive) variant: defer to the provider.
        _ => ReasoningEffort::ProviderDefault,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `None` reasoning leaves the call options untouched.
    #[test]
    fn none_is_a_noop() {
        let mut opts = CallOptions::default();
        apply_reasoning(&mut opts, None, "anthropic", "claude-opus-4-8");
        assert!(opts.reasoning.is_none());
        assert!(opts.provider_options.is_none());
    }

    /// An enabled level on a supported Anthropic model writes the
    /// `provider_options["anthropic"]` block with adaptive thinking + effort.
    #[test]
    fn anthropic_supported_writes_provider_options() {
        let mut opts = CallOptions::default();
        apply_reasoning(
            &mut opts,
            Some(ThinkingEffort::High),
            "anthropic",
            "claude-opus-4-8",
        );
        let po = opts.provider_options.expect("provider options set");
        let anthropic = po.get("anthropic").expect("anthropic slot");
        assert_eq!(anthropic["effort"], "high");
        assert_eq!(anthropic["thinking"]["type"], "adaptive");
        assert_eq!(anthropic["thinking"]["display"], "summarized");
        // The portable knob is left unset; the provider block is authoritative.
        assert!(opts.reasoning.is_none());
    }

    /// Sonnet 4.6 gets adaptive thinking + effort but NOT the Opus-only
    /// `display` field (which it returns by default / may reject).
    #[test]
    fn sonnet_4_6_enables_effort_without_display() {
        let mut opts = CallOptions::default();
        apply_reasoning(
            &mut opts,
            Some(ThinkingEffort::Medium),
            "anthropic",
            "claude-sonnet-4-6",
        );
        let po = opts.provider_options.expect("provider options set");
        let anthropic = po.get("anthropic").expect("anthropic slot");
        assert_eq!(anthropic["effort"], "medium");
        assert_eq!(anthropic["thinking"]["type"], "adaptive");
        assert!(
            anthropic["thinking"].get("display").is_none(),
            "display must be omitted for non-Opus-4.7+ models"
        );
    }

    /// `Off` on a supported Anthropic model disables thinking and sets no effort.
    #[test]
    fn anthropic_off_disables_thinking() {
        let mut opts = CallOptions::default();
        apply_reasoning(
            &mut opts,
            Some(ThinkingEffort::Off),
            "anthropic",
            "claude-opus-4-8",
        );
        let po = opts.provider_options.expect("provider options set");
        let anthropic = po.get("anthropic").expect("anthropic slot");
        assert_eq!(anthropic["thinking"]["type"], "disabled");
        assert!(anthropic.get("effort").is_none());
    }

    /// Levels map 1:1 to the effort string with no model-dependent downgrade:
    /// `Max` is sent verbatim (the UI only offers it on Opus 4.6+).
    #[test]
    fn max_maps_to_max_effort() {
        let mut opts = CallOptions::default();
        apply_reasoning(
            &mut opts,
            Some(ThinkingEffort::Max),
            "anthropic",
            "claude-opus-4-6",
        );
        let po = opts.provider_options.expect("provider options set");
        assert_eq!(po["anthropic"]["effort"], "max");
    }

    /// `Xhigh` is sent verbatim on Opus 4.8 (no downgrade anywhere).
    #[test]
    fn xhigh_maps_to_xhigh_effort() {
        let mut opts = CallOptions::default();
        apply_reasoning(
            &mut opts,
            Some(ThinkingEffort::Xhigh),
            "anthropic",
            "claude-opus-4-8",
        );
        let po = opts.provider_options.expect("provider options set");
        assert_eq!(po["anthropic"]["effort"], "xhigh");
    }

    /// A provider with no dedicated branch (xAI) uses the portable `reasoning`
    /// enum and writes no provider-options block.
    #[test]
    fn other_provider_uses_portable_enum() {
        let mut opts = CallOptions::default();
        apply_reasoning(&mut opts, Some(ThinkingEffort::Medium), "xai", "grok-4");
        assert!(opts.provider_options.is_none());
        assert_eq!(opts.reasoning, Some(ReasoningEffort::Medium));
    }

    /// OpenAI writes the effort BOTH as the portable enum (Chat path) and as
    /// `provider_options["openai"].reasoningEffort` (Responses path).
    #[test]
    fn openai_writes_both_chat_and_responses_forms() {
        let mut opts = CallOptions::default();
        apply_reasoning(&mut opts, Some(ThinkingEffort::Minimal), "openai", "gpt-5");
        assert_eq!(opts.reasoning, Some(ReasoningEffort::Minimal));
        let po = opts.provider_options.expect("provider options set");
        assert_eq!(po["openai"]["reasoningEffort"], "minimal");
    }

    /// `Off` on OpenAI omits the effort entirely (no `"none"` that `gpt-4o`
    /// rejects with a 400): neither the portable enum nor provider options set.
    #[test]
    fn openai_off_omits_effort() {
        let mut opts = CallOptions::default();
        apply_reasoning(&mut opts, Some(ThinkingEffort::Off), "openai", "gpt-4o");
        assert!(opts.reasoning.is_none());
        assert!(opts.provider_options.is_none());
    }

    /// `Xhigh` reaches the OpenAI wire verbatim (e.g. GPT-5.2+ / codex-max).
    #[test]
    fn openai_xhigh_maps_to_xhigh() {
        let mut opts = CallOptions::default();
        apply_reasoning(&mut opts, Some(ThinkingEffort::Xhigh), "openai", "gpt-5.2");
        assert_eq!(
            opts.provider_options.expect("po")["openai"]["reasoningEffort"],
            "xhigh"
        );
    }

    /// An Anthropic model that does NOT support effort (Sonnet 4.5) falls back
    /// to the portable enum so the SDK can gate it (no 400).
    #[test]
    fn anthropic_unsupported_falls_back_to_portable() {
        let mut opts = CallOptions::default();
        apply_reasoning(
            &mut opts,
            Some(ThinkingEffort::High),
            "anthropic",
            "claude-sonnet-4-5",
        );
        assert!(opts.provider_options.is_none());
        assert_eq!(opts.reasoning, Some(ReasoningEffort::High));
    }
}

// Rust guideline compliant 2026-02-21
