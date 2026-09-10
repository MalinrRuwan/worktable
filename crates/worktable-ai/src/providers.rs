//! The AI provider catalog, backed by rig.
//!
//! With `pi_agent_rust` removed, Worktable no longer inherits a provider
//! registry from the embedded agent. This module is the single owner of:
//!
//! - which providers the Settings UI offers and what they are called;
//! - the model ids each provider serves (curated from the provider's own
//!   catalog so the UI never offers an id the client cannot send);
//! - the [`ProviderKind`] the runtime matches on to construct the right rig
//!   client for a prompt.
//!
//! Worktable's own database remains the credential owner; rig's
//! `ProviderClient::from_env` is only a fallback path.

use crate::opencode_go;

/// Which rig client family a provider uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProviderKind {
    /// The custom OpenCode Go provider (OpenAI-compatible Chat Completions).
    OpenCodeGo,
    /// OpenAI's Responses API.
    OpenAi,
    /// Anthropic's Messages API.
    Anthropic,
    /// DeepSeek's OpenAI-compatible API.
    DeepSeek,
}

/// One provider entry: identity, client family, and model catalog.
#[derive(Clone, Copy, Debug)]
pub struct ProviderSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub kind: ProviderKind,
    /// `(model_id, display_name)` pairs, in catalog order.
    pub models: &'static [(&'static str, &'static str)],
}

/// OpenAI's current models (curated; ids match rig's `openai` constants).
const OPENAI_MODELS: &[(&str, &str)] = &[
    ("gpt-5.6-luna", "GPT-5.6 Luna"),
    ("gpt-5.6", "GPT-5.6"),
    ("gpt-5.5", "GPT-5.5"),
    ("gpt-5.2", "GPT-5.2"),
    ("gpt-5-mini", "GPT-5 mini"),
];

/// Anthropic's current Claude models.
const ANTHROPIC_MODELS: &[(&str, &str)] = &[
    ("claude-sonnet-4-5", "Claude Sonnet 4.5"),
    ("claude-opus-4-5", "Claude Opus 4.5"),
    ("claude-haiku-4-5", "Claude Haiku 4.5"),
];

/// DeepSeek's models (ids match rig's `deepseek` constants).
const DEEPSEEK_MODELS: &[(&str, &str)] = &[
    ("deepseek-v4-pro", "DeepSeek V4 Pro"),
    ("deepseek-v4-flash", "DeepSeek V4 Flash"),
    ("deepseek-reasoner", "DeepSeek Reasoner"),
    ("deepseek-chat", "DeepSeek Chat"),
];

/// Every provider Worktable can prompt through, in Settings order.
pub const PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        id: opencode_go::ID,
        name: opencode_go::NAME,
        kind: ProviderKind::OpenCodeGo,
        models: opencode_go::MODELS,
    },
    ProviderSpec {
        id: "openai",
        name: "OpenAI",
        kind: ProviderKind::OpenAi,
        models: OPENAI_MODELS,
    },
    ProviderSpec {
        id: "anthropic",
        name: "Anthropic",
        kind: ProviderKind::Anthropic,
        models: ANTHROPIC_MODELS,
    },
    ProviderSpec {
        id: "deepseek",
        name: "DeepSeek",
        kind: ProviderKind::DeepSeek,
        models: DEEPSEEK_MODELS,
    },
];

/// Look up a provider by its stable id.
pub fn spec(id: &str) -> Option<&'static ProviderSpec> {
    PROVIDERS.iter().find(|provider| provider.id == id)
}

/// Whether `model_id` belongs to `provider_id`'s catalog.
pub fn model_known(provider_id: &str, model_id: &str) -> bool {
    spec(provider_id).is_some_and(|provider| {
        provider
            .models
            .iter()
            .any(|(id, _)| id.eq_ignore_ascii_case(model_id))
    })
}

/// The provider's first model id, used to auto-select after an API key is
/// saved so a configured provider can immediately serve a prompt.
pub fn first_model(provider_id: &str) -> Option<&'static str> {
    spec(provider_id)?.models.first().map(|(id, _)| *id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_consistent_and_contains_opencode_go() {
        assert!(
            spec(opencode_go::ID).is_some(),
            "opencode-go must be in the catalog"
        );
        for provider in PROVIDERS {
            assert!(!provider.id.is_empty());
            assert!(!provider.name.is_empty());
            assert!(!provider.models.is_empty(), "{} has no models", provider.id);
            let mut ids: Vec<&str> = provider.models.iter().map(|(id, _)| *id).collect();
            let before = ids.len();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), before, "{} has duplicate model ids", provider.id);
        }
    }

    #[test]
    fn model_lookup_is_case_insensitive() {
        assert!(model_known("opencode-go", "GLM-5.3"));
        assert!(!model_known("opencode-go", "not-a-model"));
        assert!(!model_known("missing-provider", "glm-5.3"));
        assert_eq!(first_model("opencode-go"), Some("glm-5.3"));
    }
}
