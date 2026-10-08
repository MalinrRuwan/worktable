//! Provider identities and rig client families; models come from APIs/cache.

use crate::{chatgpt, opencode};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProviderKind {
    OpenCode,
    OpenAi,
    ChatGpt,
    Anthropic,
    DeepSeek,
}

/// Provider identity and client family, independent of discovered models.
#[derive(Clone, Copy, Debug)]
pub struct ProviderSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub kind: ProviderKind,
}

impl ProviderSpec {
    pub fn supports_api_key(&self) -> bool {
        self.kind != ProviderKind::ChatGpt
    }

    pub fn supports_oauth(&self) -> bool {
        self.kind == ProviderKind::ChatGpt
    }
}

/// Every provider Worktable can prompt through, in Settings order.
pub const PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        id: opencode::ID,
        name: opencode::NAME,
        kind: ProviderKind::OpenCode,
    },
    ProviderSpec {
        id: "openai",
        name: "OpenAI",
        kind: ProviderKind::OpenAi,
    },
    ProviderSpec {
        id: chatgpt::ID,
        name: chatgpt::NAME,
        kind: ProviderKind::ChatGpt,
    },
    ProviderSpec {
        id: "anthropic",
        name: "Anthropic",
        kind: ProviderKind::Anthropic,
    },
    ProviderSpec {
        id: "deepseek",
        name: "DeepSeek",
        kind: ProviderKind::DeepSeek,
    },
];

/// Look up a provider by its stable id.
pub fn spec(id: &str) -> Option<&'static ProviderSpec> {
    PROVIDERS.iter().find(|provider| provider.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_identities_are_unique_and_resolvable() {
        let mut ids = std::collections::HashSet::new();
        for provider in PROVIDERS {
            assert!(!provider.id.is_empty());
            assert!(!provider.name.is_empty());
            assert!(ids.insert(provider.id));
            assert_eq!(spec(provider.id).unwrap().kind, provider.kind);
        }
        assert_eq!(spec(opencode::ID).unwrap().kind, ProviderKind::OpenCode);
        assert!(spec("missing-provider").is_none());
    }

    #[test]
    fn credential_capabilities_follow_the_client_family() {
        for provider in PROVIDERS {
            let subscription = provider.kind == ProviderKind::ChatGpt;
            assert_eq!(provider.supports_oauth(), subscription);
            assert_eq!(provider.supports_api_key(), !subscription);
        }
    }
}
