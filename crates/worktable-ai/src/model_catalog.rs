//! Authenticated discovery only: a failed request never becomes a guessed catalog.
//!
//! Successful server catalogs are authoritative; model ids are never intersected
//! with a bundled list or classified by their spelling.
//! Endpoint contracts:
//! - <https://developers.openai.com/api/reference/resources/models/methods/list>
//! - <https://api-docs.deepseek.com/api/list-models>
//! - <https://platform.claude.com/docs/en/api/models/list> (2023-06-01)
//! - <https://opencode.ai/docs/go/> and <https://opencode.ai/docs/zen/>
//! - <https://models.opencode.ai/api.json> for OpenCode endpoint metadata
//! - Codex `codex-api/src/endpoint/models.rs` and `protocol/src/openai_models.rs`
//!   (<https://github.com/openai/codex/tree/main/codex-rs>).

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use reqwest::{Client, RequestBuilder, StatusCode};
use serde::Deserialize;
use worktable_db::SqliteStore;
use worktable_events::ModelInfo;

use crate::{
    chatgpt,
    model_metadata::{self, ModelMetadata},
    opencode, providers,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_PAGES: usize = 50;
// Discovery is not a conversation; all refreshes use the same routing identity.
const CATALOG_SESSION: &str = "worktable-model-catalog";

/// A successful catalog can carry an enabled service's safe discovery warning.
#[derive(Debug)]
pub struct FetchedModels {
    pub models: Vec<ModelInfo>,
    pub warning: Option<String>,
}

/// Discover only models actually advertised to this stored credential.
/// No environment credentials or other applications' auth files are consulted.
pub async fn fetch(store: SqliteStore, provider_id: String) -> Result<FetchedModels, String> {
    tokio::time::timeout(DISCOVERY_TIMEOUT, fetch_inner(store, provider_id))
        .await
        .map_err(|_| "Model discovery timed out. Check your connection and retry.".to_string())?
}

async fn fetch_inner(store: SqliteStore, provider_id: String) -> Result<FetchedModels, String> {
    providers::spec(&provider_id)
        .ok_or_else(|| "Unknown provider. Choose a configured provider.".to_string())?;
    let client = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Could not start model discovery. Retry.".to_string())?;
    if provider_id == "chatgpt-subscription" {
        let auth = chatgpt::auth_context(&store).await?;
        let mut request = client
            .get(format!("{}/models", chatgpt::API_BASE_URL))
            .query(&[("client_version", env!("CARGO_PKG_VERSION"))])
            .bearer_auth(auth.access_token)
            .header("originator", "worktable");
        if let Some(account_id) = auth.account_id {
            request = request.header("chatgpt-account-id", account_id);
        }
        let body = response_body(request).await?;
        return fetched(subscription_models(&body)?, Vec::new());
    }

    let services = opencode::Services::from_store(&store);
    if provider_id == opencode::ID && !services.go && !services.zen {
        return Ok(FetchedModels {
            models: Vec::new(),
            warning: None,
        });
    }
    let credential = store
        .read_provider_credential(&provider_id)
        .map_err(|_| {
            "Could not read provider credentials. Configure the provider again.".to_string()
        })?
        .filter(|credential| credential.kind == "api_key" && !credential.key.trim().is_empty())
        .ok_or_else(|| "No API key configured. Add a key in Providers and retry.".to_string())?;
    if provider_id == opencode::ID {
        let mut rows = Vec::new();
        let mut warnings = Vec::new();
        // `/models` lists ids only. One process-wide metadata fetch supplies
        // every row's dialect, display name, and price; a failed fetch must not
        // turn into a guessed protocol.
        let metadata = match model_metadata::load().await {
            Ok(metadata) => Some(metadata),
            Err(error) => {
                warnings.push(error);
                model_metadata::cached()
            }
        };
        for service in opencode::SERVICES {
            let enabled = match service {
                opencode::Service::Go => services.go,
                opencode::Service::Zen => services.zen,
            };
            if !enabled {
                continue;
            }
            let base = match service {
                opencode::Service::Go => opencode::GO_BASE_URL,
                opencode::Service::Zen => opencode::ZEN_BASE_URL,
            };
            let response = response_body(
                client
                    .get(format!("{base}/models"))
                    .bearer_auth(&credential.key)
                    .header(opencode::SESSION_HEADER, CATALOG_SESSION),
            )
            .await;
            let page = response.and_then(|body| parse_page(&body));
            let advertised = match page {
                Ok(page) => advertised_models(page.data, Some(service)),
                Err(error) => {
                    warnings.push(format!("{}: {error}", service.name()));
                    continue;
                }
            };
            if advertised.is_empty() {
                warnings.push(format!("{} returned no models.", service.name()));
            }
            // Preserve the advertising gateway; Go wins only shared advertisements.
            rows.extend(annotate_opencode(service, advertised, metadata));
        }
        return fetched(deduplicate(rows), warnings);
    }

    let endpoint = match provider_id.as_str() {
        "openai" => "https://api.openai.com/v1/models",
        "deepseek" => "https://api.deepseek.com/models",
        "anthropic" => "https://api.anthropic.com/v1/models",
        _ => return Err("Model discovery is not supported for this provider.".to_string()),
    };
    let anthropic = provider_id == "anthropic";
    let mut cursor = None;
    let mut seen_cursors = HashSet::new();
    let mut rows = Vec::new();
    for _ in 0..MAX_PAGES {
        let mut request = client.get(endpoint);
        if anthropic {
            request = request
                .header("x-api-key", &credential.key)
                .header("anthropic-version", "2023-06-01")
                .query(&[("limit", "1000")]);
            if let Some(after_id) = &cursor {
                request = request.query(&[("after_id", after_id)]);
            }
        } else {
            request = request.bearer_auth(&credential.key);
        }
        let body = response_body(request).await?;
        let page = parse_page(&body)?;
        rows.extend(advertised_models(page.data, None));
        if !anthropic || !page.has_more {
            return fetched(deduplicate(rows), Vec::new());
        }
        let next = page
            .last_id
            .filter(|id| !id.trim().is_empty() && seen_cursors.insert(id.clone()))
            .ok_or_else(|| "Invalid model pagination. Retry model discovery.".to_string())?;
        cursor = Some(next);
    }
    Err("Model catalog exceeded the page limit. Retry model discovery.".to_string())
}

async fn response_body(request: RequestBuilder) -> Result<Vec<u8>, String> {
    let mut response = request
        .send()
        .await
        .map_err(|_| "Could not fetch models. Check your connection and retry.".to_string())?;
    if !response.status().is_success() {
        return Err(http_error(response.status()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Could not read the model catalog. Retry.".to_string())?
    {
        if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            return Err("Model catalog is too large. Retry model discovery.".to_string());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn http_error(status: StatusCode) -> String {
    let recovery = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            "Check your provider credentials or subscription and retry."
        }
        StatusCode::TOO_MANY_REQUESTS => "Wait a moment and retry.",
        _ => "Check provider availability and retry.",
    };
    format!(
        "Model discovery failed (HTTP {}). {recovery}",
        status.as_u16()
    )
}

#[derive(Deserialize)]
struct ModelPage {
    data: Vec<ApiModel>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    last_id: Option<String>,
}

#[derive(Deserialize)]
struct ApiModel {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    api: Option<String>,
}

fn parse_page(body: &[u8]) -> Result<ModelPage, String> {
    let page: ModelPage = serde_json::from_slice(body)
        .map_err(|_| "Invalid model catalog returned by provider. Retry.".to_string())?;
    if page.data.iter().any(|model| model.id.trim().is_empty()) {
        return Err("Model catalog contains an empty model id. Retry.".to_string());
    }
    Ok(page)
}

fn advertised_models(models: Vec<ApiModel>, service: Option<opencode::Service>) -> Vec<ModelInfo> {
    models
        .into_iter()
        .map(|model| ModelInfo {
            name: model
                .display_name
                .or(model.name)
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| model.id.clone()),
            id: model.id,
            group: service.map(|service| service.name().to_string()),
            api: model.api,
            free: false,
        })
        .collect()
}

/// Annotate the rows one OpenCode gateway actually advertised.
///
/// Zen's free models are restricted to OpenCode's own client, so they are not
/// offered here. Go's free models are callable and stay. Rows without metadata
/// or with an unsupported dialect are hidden rather than guessed; metadata
/// never adds a row the `/models` response did not advertise.
fn annotate_opencode(
    service: opencode::Service,
    rows: Vec<ModelInfo>,
    metadata: Option<&ModelMetadata>,
) -> Vec<ModelInfo> {
    let Some(metadata) = metadata else {
        return Vec::new();
    };
    rows.into_iter()
        .filter_map(|mut row| {
            let entry = metadata.entry(service, &row.id)?;
            let api = entry.api()?;
            if service == opencode::Service::Zen && entry.free {
                return None;
            }
            if let Some(name) = entry.name.as_ref() {
                row.name = name.clone();
            }
            row.api = Some(api.to_owned());
            row.free = entry.free;
            Some(row)
        })
        .collect()
}

/// Default selection for a provider's offered rows.
///
/// Paid Chat Completions rows come first; a free row is never auto-selected
/// while a paid Chat row exists. The catalog is already sorted paid-first, but
/// the explicit passes keep the intent independent of that ordering.
pub(crate) fn default_model(models: &[ModelInfo]) -> Option<&ModelInfo> {
    models
        .iter()
        .find(|model| !model.free && model.api.as_deref() == Some("chat"))
        .or_else(|| {
            models
                .iter()
                .find(|model| model.api.as_deref() == Some("chat"))
        })
        .or_else(|| models.first())
}

#[derive(Deserialize)]
struct SubscriptionCatalog {
    models: Vec<SubscriptionModel>,
}

#[derive(Deserialize)]
struct SubscriptionModel {
    slug: String,
    display_name: String,
    #[serde(default)]
    api: Option<String>,
}

fn subscription_models(body: &[u8]) -> Result<Vec<ModelInfo>, String> {
    let catalog: SubscriptionCatalog = serde_json::from_slice(body)
        .map_err(|_| "Invalid subscription model catalog. Retry.".to_string())?;
    let mut rows = Vec::new();
    for model in catalog.models {
        if model.slug.trim().is_empty() || model.display_name.trim().is_empty() {
            return Err(
                "Subscription catalog contains an empty model id or name. Retry.".to_string(),
            );
        }
        rows.push(ModelInfo {
            id: model.slug,
            name: model.display_name,
            group: None,
            api: model.api,
            free: false,
        });
    }
    Ok(deduplicate(rows))
}

fn deduplicate(rows: Vec<ModelInfo>) -> Vec<ModelInfo> {
    let mut models = BTreeMap::new();
    // First advertisement wins; callers visit Go before Zen.
    for row in rows {
        models.entry(row.id.clone()).or_insert(row);
    }
    let mut rows: Vec<_> = models.into_values().collect();
    rows.sort_by(|a, b| {
        let rank = |row: &ModelInfo| match row.group.as_deref() {
            Some("OpenCode Go") => 0,
            Some("OpenCode Zen") => 1,
            _ => 2,
        };
        a.free
            .cmp(&b.free)
            .then_with(|| rank(a).cmp(&rank(b)))
            .then_with(|| a.id.cmp(&b.id))
    });
    rows
}

fn nonempty(rows: Vec<ModelInfo>) -> Result<Vec<ModelInfo>, String> {
    if rows.is_empty() {
        Err("No models returned. Check provider access and refresh models.".to_string())
    } else {
        Ok(rows)
    }
}

fn fetched(rows: Vec<ModelInfo>, warnings: Vec<String>) -> Result<FetchedModels, String> {
    let warning = (!warnings.is_empty()).then(|| warnings.join(" "));
    let models = nonempty(rows).map_err(|error| warning.clone().unwrap_or(error))?;
    Ok(FetchedModels { models, warning })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opencode_metadata() -> ModelMetadata {
        model_metadata::parse(
            br#"{
                "opencode": {
                    "id": "opencode",
                    "npm": "@ai-sdk/openai-compatible",
                    "api": "https://opencode.ai/inference/openai/v1",
                    "models": {
                        "zen-free": {"id": "zen-free", "name": "Zen Free", "cost": {"input": 0, "output": 0}},
                        "zen-paid": {"id": "zen-paid", "name": "Zen Paid", "cost": {"input": 1, "output": 2}},
                        "zen-claude": {"id": "zen-claude", "name": "Zen Claude", "cost": {"input": 2, "output": 10}, "provider": {"npm": "@ai-sdk/anthropic"}},
                        "zen-responses": {"id": "zen-responses", "name": "Zen Responses", "cost": {"input": 2, "output": 12}, "provider": {"npm": "@ai-sdk/openai"}},
                        "zen-gemini": {"id": "zen-gemini", "name": "Zen Gemini", "cost": {"input": 2, "output": 12}, "provider": {"npm": "@ai-sdk/google"}},
                        "zen-metadata-only": {"id": "zen-metadata-only", "name": "Not advertised", "cost": {"input": 1, "output": 2}}
                    }
                },
                "opencode-go": {
                    "id": "opencode-go",
                    "npm": "@ai-sdk/openai-compatible",
                    "api": "https://opencode.ai/inference/go/openai/v1",
                    "models": {
                        "go-free": {"id": "go-free", "name": "Go Free", "cost": {"input": 0, "output": 0}},
                        "go-paid": {"id": "go-paid", "name": "Go Paid", "cost": {"input": 1, "output": 2}},
                        "go-messages": {"id": "go-messages", "name": "Go Messages", "cost": {"input": 2, "output": 10}, "provider": {"npm": "@ai-sdk/anthropic"}},
                        "go-gemini": {"id": "go-gemini", "name": "Go Gemini", "cost": {"input": 2, "output": 10}, "provider": {"npm": "@ai-sdk/google"}}
                    }
                }
            }"#,
        )
        .expect("metadata fixture")
    }

    fn advertised(service: opencode::Service, id: &str) -> ModelInfo {
        ModelInfo {
            id: id.to_owned(),
            name: format!("Advertised {id}"),
            group: Some(service.name().to_owned()),
            api: None,
            free: false,
        }
    }

    #[test]
    fn zen_free_unknown_and_unsupported_rows_are_hidden() {
        let metadata = opencode_metadata();
        let rows = annotate_opencode(
            opencode::Service::Zen,
            vec![
                advertised(opencode::Service::Zen, "zen-free"),
                advertised(opencode::Service::Zen, "zen-paid"),
                advertised(opencode::Service::Zen, "zen-unknown"),
                advertised(opencode::Service::Zen, "zen-claude"),
                advertised(opencode::Service::Zen, "zen-responses"),
                advertised(opencode::Service::Zen, "zen-gemini"),
            ],
            Some(&metadata),
        );
        let ids: Vec<_> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["zen-paid", "zen-claude", "zen-responses"]);
        assert_eq!(rows[0].name, "Zen Paid");
        assert_eq!(rows[0].api.as_deref(), Some("chat"));
        assert!(!rows[0].free);
        assert_eq!(rows[1].api.as_deref(), Some("messages"));
        assert_eq!(rows[2].api.as_deref(), Some("responses"));
        assert!(
            !rows.iter().any(|row| row.id == "zen-metadata-only"),
            "metadata must never add an unadvertised model"
        );
    }

    #[test]
    fn go_keeps_free_rows_and_supported_paid_rows() {
        let metadata = opencode_metadata();
        let rows = annotate_opencode(
            opencode::Service::Go,
            vec![
                advertised(opencode::Service::Go, "go-free"),
                advertised(opencode::Service::Go, "go-paid"),
                advertised(opencode::Service::Go, "go-messages"),
                advertised(opencode::Service::Go, "go-gemini"),
                advertised(opencode::Service::Go, "go-unknown"),
            ],
            Some(&metadata),
        );
        let ids: Vec<_> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["go-free", "go-paid", "go-messages"]);
        assert!(rows[0].free);
        assert_eq!(rows[0].api.as_deref(), Some("chat"));
        assert_eq!(rows[2].api.as_deref(), Some("messages"));
    }

    #[test]
    fn missing_metadata_keeps_the_catalog_empty_with_a_recovery_error() {
        let rows = annotate_opencode(
            opencode::Service::Zen,
            vec![advertised(opencode::Service::Zen, "zen-paid")],
            None,
        );
        assert!(
            rows.is_empty(),
            "unknown metadata must not fall back to Chat"
        );
        let error = model_metadata::parse(b"not-json").unwrap_err();
        let error = fetched(rows, vec![error]).unwrap_err();
        assert!(error.contains("metadata"), "unexpected error: {error}");
        assert!(!error.contains("not-json"));
    }

    #[test]
    fn paid_rows_sort_before_free_rows_then_group_then_id() {
        let rows = deduplicate(vec![
            ModelInfo {
                id: "zen-paid".into(),
                name: "Zen paid".into(),
                group: Some(opencode::Service::Zen.name().into()),
                api: Some("chat".into()),
                free: false,
            },
            ModelInfo {
                id: "go-free".into(),
                name: "Go free".into(),
                group: Some(opencode::Service::Go.name().into()),
                api: Some("chat".into()),
                free: true,
            },
            ModelInfo {
                id: "go-paid".into(),
                name: "Go paid".into(),
                group: Some(opencode::Service::Go.name().into()),
                api: Some("chat".into()),
                free: false,
            },
        ]);
        let ids: Vec<_> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["go-paid", "zen-paid", "go-free"]);
    }

    #[test]
    fn default_selection_prefers_a_paid_chat_model() {
        let models = vec![
            ModelInfo {
                id: "zen-free".into(),
                name: "Zen free".into(),
                group: Some(opencode::Service::Zen.name().into()),
                api: Some("chat".into()),
                free: true,
            },
            ModelInfo {
                id: "zen-paid".into(),
                name: "Zen paid".into(),
                group: Some(opencode::Service::Zen.name().into()),
                api: Some("chat".into()),
                free: false,
            },
            ModelInfo {
                id: "go-free".into(),
                name: "Go free".into(),
                group: Some(opencode::Service::Go.name().into()),
                api: Some("chat".into()),
                free: true,
            },
        ];
        assert_eq!(
            default_model(&models).map(|model| model.id.as_str()),
            Some("zen-paid"),
            "a free Zen model must not win the default"
        );

        let no_chat = vec![ModelInfo {
            id: "responses".into(),
            name: "Responses".into(),
            group: None,
            api: Some("responses".into()),
            free: false,
        }];
        assert_eq!(
            default_model(&no_chat).map(|model| model.id.as_str()),
            Some("responses")
        );
        assert!(default_model(&[]).is_none());
    }

    #[test]
    fn api_catalog_preserves_all_raw_ids_and_server_names() {
        let page = parse_page(br#"{"data":[{"id":"arbitrary/a","name":"Server name","api":"arbitrary-protocol"},{"id":"arbitrary/a"},{"id":"arbitrary/b"}]}"#).unwrap();
        let rows = deduplicate(advertised_models(page.data, None));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "arbitrary/a");
        assert_eq!(rows[0].name, "Server name");
        assert_eq!(rows[0].api.as_deref(), Some("arbitrary-protocol"));
        assert!(rows[0].group.is_none());
        assert_eq!(rows[1].name, "arbitrary/b");
        assert!(rows[1].api.is_none());
    }

    #[test]
    fn anthropic_page_preserves_cursor_and_server_display_name() {
        let page = parse_page(
            br#"{"data":[{"id":"arbitrary-model","display_name":"Server name"}],"has_more":true,"last_id":"arbitrary-model"}"#,
        )
        .unwrap();
        assert!(page.has_more);
        assert_eq!(page.last_id.as_deref(), Some("arbitrary-model"));
        let rows = advertised_models(page.data, None);
        assert_eq!(rows[0].name, "Server name");
    }

    #[test]
    fn opencode_groups_come_from_endpoint_and_go_wins_shared_ids() {
        let go = advertised_models(
            parse_page(br#"{"data":[{"id":"shared","name":"Go name"}]}"#)
                .unwrap()
                .data,
            Some(opencode::Service::Go),
        );
        let zen = advertised_models(
            parse_page(br#"{"data":[{"id":"shared","name":"Zen name"},{"id":"zen-only"}]}"#)
                .unwrap()
                .data,
            Some(opencode::Service::Zen),
        );
        let rows = deduplicate(go.into_iter().chain(zen).collect());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "shared");
        assert_eq!(rows[0].name, "Go name");
        assert_eq!(rows[0].group.as_deref(), Some("OpenCode Go"));
        assert_eq!(rows[1].id, "zen-only");
        assert_eq!(rows[1].group.as_deref(), Some("OpenCode Zen"));
    }

    #[test]
    fn partial_catalog_warns_without_inventing_gateway_failover() {
        let rows = advertised_models(
            parse_page(br#"{"data":[{"id":"arbitrary-model"}]}"#)
                .unwrap()
                .data,
            Some(opencode::Service::Zen),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "arbitrary-model");
        assert_eq!(rows[0].group.as_deref(), Some("OpenCode Zen"));
        let result = fetched(rows, vec!["OpenCode Go: HTTP 403".to_string()]).unwrap();
        assert_eq!(result.models.len(), 1);
        assert!(result.warning.unwrap().contains("OpenCode Go"));
        assert!(fetched(Vec::new(), vec!["OpenCode Go: HTTP 403".to_string()]).is_err());
    }

    #[test]
    fn subscription_catalog_keeps_all_server_slugs_regardless_of_capabilities() {
        let rows = subscription_models(br#"{"models":[{"slug":"raw-model","display_name":"Server model","visibility":"list","supported_in_api":false},{"slug":"hidden","display_name":"Hidden","visibility":"hide"},{"slug":"audio","display_name":"Audio","visibility":"list","input_modalities":["audio"]}]}"#).unwrap();
        assert_eq!(rows.len(), 3);
        let row = rows.iter().find(|row| row.id == "raw-model").unwrap();
        assert_eq!(row.name, "Server model");
    }

    #[test]
    fn malformed_and_empty_catalogs_fail_without_echoing_input() {
        for body in [
            b"secret-token".as_slice(),
            br#"{"data":[{"id":""}]}"#,
            br#"{}"#,
        ] {
            let error = parse_page(body).err().unwrap();
            assert!(!error.contains("secret-token"));
        }
        assert!(nonempty(Vec::new()).is_err());
        assert!(
            subscription_models(
                br#"{"models":[{"slug":"","display_name":"Name","visibility":"list"}]}"#
            )
            .is_err()
        );
        assert!(http_error(StatusCode::UNAUTHORIZED).contains("credentials"));
        assert!(http_error(StatusCode::TOO_MANY_REQUESTS).contains("Wait"));
    }
}
