//! OpenCode's live model metadata: API dialect, display name, and price.
//!
//! The `/models` listing only advertises ids, so a third-party client cannot
//! know whether a row speaks Chat Completions, Responses, or Messages. The
//! catalog at <https://models.opencode.ai/api.json> carries that endpoint
//! metadata per provider (`opencode`, `opencode-go`) and an optional per-model
//! override. It is fetched once per process; routing never guesses from ids.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::OnceCell;

use crate::opencode::{self, Dialect};

/// Same catalog models.dev serves; OpenCode exposes it under its own host.
pub const METADATA_URL: &str = "https://models.opencode.ai/api.json";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Provider ids inside the metadata catalog, not Worktable provider ids.
const ZEN_PROVIDER_ID: &str = "opencode";
const GO_PROVIDER_ID: &str = "opencode-go";

const METADATA_UNAVAILABLE: &str =
    "OpenCode model metadata is unavailable. Check your connection and retry model discovery.";

/// One model's endpoint metadata. `dialect` is `None` when Worktable has no
/// client for that provider adapter, so callers must hide the row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetadataModel {
    pub name: Option<String>,
    pub free: bool,
    pub dialect: Option<Dialect>,
}

impl MetadataModel {
    /// The canonical `ModelInfo.api` value for this dialect.
    pub fn api(&self) -> Option<&'static str> {
        self.dialect.map(Dialect::as_str)
    }
}

/// Parsed metadata for the two OpenCode providers only.
#[derive(Clone, Debug, Default)]
pub struct ModelMetadata {
    providers: BTreeMap<String, BTreeMap<String, MetadataModel>>,
}

impl ModelMetadata {
    /// Look up one advertised model under the gateway that serves it.
    pub fn entry(&self, service: opencode::Service, model_id: &str) -> Option<&MetadataModel> {
        self.providers
            .get(provider_id(service))?
            .get(model_id.trim())
    }

    #[cfg(test)]
    fn provider(&self, service: opencode::Service) -> &BTreeMap<String, MetadataModel> {
        self.providers
            .get(provider_id(service))
            .expect("fixture provider")
    }
}

fn provider_id(service: opencode::Service) -> &'static str {
    match service {
        opencode::Service::Go => GO_PROVIDER_ID,
        opencode::Service::Zen => ZEN_PROVIDER_ID,
    }
}

/// Classify a provider adapter and endpoint.
///
/// - Anthropic adapter or an `/anthropic/` endpoint path -> Messages.
/// - `@ai-sdk/openai` -> Responses.
/// - `@ai-sdk/openai-compatible`, or no adapter metadata -> Chat.
/// - Google, Vertex, systemone, or any other adapter -> unsupported.
pub fn classify(npm: Option<&str>, api: Option<&str>) -> Option<Dialect> {
    if npm == Some("@ai-sdk/anthropic") || api.is_some_and(|api| has_path_segment(api, "anthropic"))
    {
        return Some(Dialect::Messages);
    }
    if api.is_some_and(|api| {
        ["google", "google-vertex", "systemone"]
            .iter()
            .any(|needle| has_path_segment(api, needle))
    }) {
        return None;
    }
    match npm {
        Some("@ai-sdk/openai") => Some(Dialect::Responses),
        Some("@ai-sdk/openai-compatible") => Some(Dialect::Chat),
        Some(_) => None,
        None => Some(Dialect::Chat),
    }
}

fn has_path_segment(api: &str, needle: &str) -> bool {
    api.split('/')
        .any(|segment| segment.eq_ignore_ascii_case(needle))
}

/// Parse the live metadata document. Tests feed a fixture; production feeds
/// the HTTP body. Never contacts the network.
pub fn parse(body: &[u8]) -> Result<ModelMetadata, String> {
    let catalog: Catalog = serde_json::from_slice(body)
        .map_err(|_| "Invalid OpenCode model metadata. Retry model discovery.".to_string())?;
    let mut metadata = ModelMetadata::default();
    for service in [opencode::Service::Zen, opencode::Service::Go] {
        let Some(provider) = catalog.providers.get(provider_id(service)) else {
            continue;
        };
        let models = provider
            .models
            .iter()
            .filter(|(id, _)| !id.trim().is_empty())
            .map(|(id, model)| {
                let npm = model
                    .provider
                    .as_ref()
                    .and_then(|provider| provider.npm.as_deref())
                    .or(provider.npm.as_deref());
                let api = model
                    .provider
                    .as_ref()
                    .and_then(|provider| provider.api.as_deref())
                    .or(provider.api.as_deref());
                let entry = MetadataModel {
                    name: model.name.clone().filter(|name| !name.trim().is_empty()),
                    free: model.cost.as_ref().is_some_and(Cost::is_free),
                    dialect: classify(npm, api),
                };
                (id.clone(), entry)
            })
            .collect();
        metadata
            .providers
            .insert(provider_id(service).to_owned(), models);
    }
    if metadata.providers.is_empty() {
        return Err(
            "OpenCode model metadata has no OpenCode catalogs. Retry model discovery.".to_string(),
        );
    }
    Ok(metadata)
}

/// Process-wide parsed metadata. `get_or_try_init` retries after a failure and
/// keeps a success for the lifetime of the process, so this is not a per-model
/// or per-Settings-open fetch.
pub async fn load() -> Result<&'static ModelMetadata, String> {
    METADATA.get_or_try_init(fetch).await
}

/// Metadata already loaded in this process, if any. Used only as a fallback
/// when a retry fails; callers must not invent a dialect without it.
pub fn cached() -> Option<&'static ModelMetadata> {
    METADATA.get()
}

static METADATA: OnceCell<ModelMetadata> = OnceCell::const_new();

async fn fetch() -> Result<ModelMetadata, String> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| METADATA_UNAVAILABLE.to_string())?;
    let response = tokio::time::timeout(REQUEST_TIMEOUT, client.get(METADATA_URL).send())
        .await
        .map_err(|_| METADATA_UNAVAILABLE.to_string())?
        .map_err(|_| METADATA_UNAVAILABLE.to_string())?;
    if !response.status().is_success() {
        return Err(METADATA_UNAVAILABLE.to_string());
    }
    let body = tokio::time::timeout(REQUEST_TIMEOUT, response.bytes())
        .await
        .map_err(|_| METADATA_UNAVAILABLE.to_string())?
        .map_err(|_| METADATA_UNAVAILABLE.to_string())?;
    if body.len() > MAX_BODY_BYTES {
        return Err("OpenCode model metadata is too large. Retry model discovery.".to_string());
    }
    parse(&body)
}

#[derive(Deserialize)]
struct Catalog {
    #[serde(flatten)]
    providers: BTreeMap<String, Provider>,
}

#[derive(Deserialize)]
struct Provider {
    #[serde(default)]
    npm: Option<String>,
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, Model>,
}

#[derive(Deserialize)]
struct Model {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    cost: Option<Cost>,
    /// Per-model adapter override. The live catalog mostly relies on the
    /// provider default and uses this for Claude/Gemini/OpenAI models.
    #[serde(default)]
    provider: Option<Override>,
}

#[derive(Deserialize)]
struct Override {
    #[serde(default)]
    npm: Option<String>,
    #[serde(default)]
    api: Option<String>,
}

#[derive(Deserialize)]
struct Cost {
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
}

impl Cost {
    fn is_free(&self) -> bool {
        self.input == Some(0.0) && self.output == Some(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
        "opencode": {
            "id": "opencode",
            "npm": "@ai-sdk/openai-compatible",
            "api": "https://opencode.ai/inference/openai/v1",
            "name": "OpenCode",
            "models": {
                "big-pickle": {
                    "id": "big-pickle",
                    "name": "Big Pickle",
                    "cost": {"input": 0, "output": 0}
                },
                "paid-chat": {
                    "id": "paid-chat",
                    "name": "Paid Chat",
                    "cost": {"input": 1.4, "output": 4.4}
                },
                "claude-sonnet": {
                    "id": "claude-sonnet",
                    "name": "Claude Sonnet",
                    "cost": {"input": 2, "output": 10},
                    "provider": {
                        "npm": "@ai-sdk/anthropic",
                        "api": "https://opencode.ai/inference/anthropic/v1"
                    }
                },
                "gpt-responses": {
                    "id": "gpt-responses",
                    "name": "GPT Responses",
                    "cost": {"input": 2, "output": 12},
                    "provider": {"npm": "@ai-sdk/openai"}
                },
                "gemini": {
                    "id": "gemini",
                    "name": "Gemini",
                    "cost": {"input": 2, "output": 12},
                    "provider": {"npm": "@ai-sdk/google"}
                },
                "system-one": {
                    "id": "system-one",
                    "name": "System One",
                    "cost": {"input": 0, "output": 0},
                    "provider": {"npm": "@ai-sdk/systemone"}
                }
            }
        },
        "opencode-go": {
            "id": "opencode-go",
            "npm": "@ai-sdk/openai-compatible",
            "api": "https://opencode.ai/inference/go/openai/v1",
            "name": "OpenCode Go",
            "models": {
                "beam-free": {
                    "id": "beam-free",
                    "name": "Beam Free",
                    "cost": {"input": 0, "output": 0}
                },
                "paid-messages": {
                    "id": "paid-messages",
                    "name": "Paid Messages",
                    "cost": {"input": 1, "output": 2},
                    "provider": {"npm": "@ai-sdk/anthropic"}
                }
            }
        },
        "unrelated-provider": {
            "id": "unrelated-provider",
            "npm": "@ai-sdk/openai",
            "models": {"not-indexed": {"id": "not-indexed"}}
        }
    }"#;

    fn fixture() -> ModelMetadata {
        parse(FIXTURE.as_bytes()).expect("fixture should parse")
    }

    #[test]
    fn classifies_supported_adapters_and_endpoints() {
        assert_eq!(
            classify(Some("@ai-sdk/openai-compatible"), None),
            Some(Dialect::Chat)
        );
        assert_eq!(classify(None, None), Some(Dialect::Chat));
        assert_eq!(
            classify(Some("@ai-sdk/openai"), None),
            Some(Dialect::Responses)
        );
        assert_eq!(
            classify(Some("@ai-sdk/anthropic"), None),
            Some(Dialect::Messages)
        );
        assert_eq!(
            classify(None, Some("https://opencode.ai/inference/anthropic/v1")),
            Some(Dialect::Messages)
        );
        for npm in [
            "@ai-sdk/google",
            "@ai-sdk/google-vertex",
            "@ai-sdk/systemone",
        ] {
            assert_eq!(classify(Some(npm), None), None, "{npm} has no client");
        }
        assert_eq!(
            classify(None, Some("https://opencode.ai/inference/google/v1beta")),
            None
        );
        assert_eq!(classify(Some("@ai-sdk/mistral"), None), None);
    }

    #[test]
    fn parse_indexes_only_the_two_opencode_providers() {
        let metadata = fixture();
        let zen = metadata.provider(opencode::Service::Zen);
        assert_eq!(zen.len(), 6);
        assert!(
            metadata
                .entry(opencode::Service::Zen, "unrelated-provider")
                .is_none()
        );
        assert!(
            metadata
                .entry(opencode::Service::Go, "not-indexed")
                .is_none()
        );

        let paid = metadata.entry(opencode::Service::Zen, "paid-chat").unwrap();
        assert_eq!(paid.name.as_deref(), Some("Paid Chat"));
        assert!(!paid.free);
        assert_eq!(paid.api(), Some("chat"));

        let claude = metadata
            .entry(opencode::Service::Zen, "claude-sonnet")
            .unwrap();
        assert_eq!(claude.api(), Some("messages"));

        let responses = metadata
            .entry(opencode::Service::Zen, "gpt-responses")
            .unwrap();
        assert_eq!(responses.api(), Some("responses"));

        assert!(
            metadata
                .entry(opencode::Service::Zen, "gemini")
                .unwrap()
                .dialect
                .is_none()
        );
        assert!(
            metadata
                .entry(opencode::Service::Zen, "system-one")
                .unwrap()
                .dialect
                .is_none()
        );
    }

    #[test]
    fn free_requires_both_costs_to_be_zero() {
        let metadata = fixture();
        assert!(
            metadata
                .entry(opencode::Service::Zen, "big-pickle")
                .unwrap()
                .free
        );
        assert!(
            metadata
                .entry(opencode::Service::Go, "beam-free")
                .unwrap()
                .free
        );
        assert!(
            !metadata
                .entry(opencode::Service::Zen, "paid-chat")
                .unwrap()
                .free
        );
    }

    #[test]
    fn malformed_or_foreign_metadata_fails_without_echoing_input() {
        for body in [
            b"secret-token".as_slice(),
            b"[]".as_slice(),
            br#"{"unrelated-provider": {"models": {}}}"#.as_slice(),
        ] {
            let error = parse(body).expect_err("invalid metadata should fail");
            assert!(!error.contains("secret-token"));
        }
    }
}
