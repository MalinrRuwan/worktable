//! OpenCode gateway configuration, metadata routing, and rig clients.
//!
//! Discovery owns model membership and resolves shared ids in favor of Go.
//! Routing consumes endpoint-owned metadata, never model ids or prefixes.

use anyhow::anyhow;
use rig::http_client::{HeaderMap, HeaderValue};
use rig::providers::{anthropic, openai};
use worktable_db::SqliteStore;

/// Stable provider id retained from the Go-only era for saved credentials.
pub const ID: &str = "opencode-go";
pub const NAME: &str = "OpenCode";
pub const GO_BASE_URL: &str = "https://opencode.ai/zen/go/v1";
pub const ZEN_BASE_URL: &str = "https://opencode.ai/zen/v1";
/// Environment fallback; the app normally passes the stored key.
pub const API_KEY_ENV: &str = "OPENCODE_GO_API_KEY";
/// Stable session id required by Go and accepted by Zen.
pub const SESSION_HEADER: &str = "x-opencode-session";
/// Missing service flags default to enabled.
pub const GO_ENABLED_KEY: &str = "opencode_go_enabled";
pub const ZEN_ENABLED_KEY: &str = "opencode_zen_enabled";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Service {
    Go,
    Zen,
}

impl Service {
    pub const fn id(self) -> &'static str {
        match self {
            Service::Go => "go",
            Service::Zen => "zen",
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Service::Go => "OpenCode Go",
            Service::Zen => "OpenCode Zen",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "go" => Some(Service::Go),
            "zen" => Some(Service::Zen),
            _ => None,
        }
    }

    pub const fn base_url(self) -> &'static str {
        match self {
            Service::Go => GO_BASE_URL,
            Service::Zen => ZEN_BASE_URL,
        }
    }
}

pub const SERVICES: [Service; 2] = [Service::Go, Service::Zen];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dialect {
    Chat,
    Responses,
    Messages,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Services {
    pub go: bool,
    pub zen: bool,
}

impl Default for Services {
    fn default() -> Self {
        Self {
            go: true,
            zen: true,
        }
    }
}

impl Services {
    pub const fn enabled(self, service: Service) -> bool {
        match service {
            Service::Go => self.go,
            Service::Zen => self.zen,
        }
    }

    /// Missing keys keep the default (enabled).
    pub fn from_store(store: &SqliteStore) -> Self {
        let stored = |key: &str| store.get_config(key).ok().flatten();
        Self {
            go: stored(GO_ENABLED_KEY).is_none_or(|value| value != "0"),
            zen: stored(ZEN_ENABLED_KEY).is_none_or(|value| value != "0"),
        }
    }

    pub fn write_to_store(self, store: &SqliteStore) -> anyhow::Result<()> {
        store.set_config(GO_ENABLED_KEY, if self.go { "1" } else { "0" })?;
        store.set_config(ZEN_ENABLED_KEY, if self.zen { "1" } else { "0" })?;
        Ok(())
    }

    pub fn with(self, service: Service, enabled: bool) -> Self {
        match service {
            Service::Go => Self {
                go: enabled,
                ..self
            },
            Service::Zen => Self {
                zen: enabled,
                ..self
            },
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Route {
    pub service: Service,
    pub dialect: Dialect,
}

/// Route from the API endpoint's service heading and advertised dialect.
///
/// Disabled services are rejected, not replaced by another gateway. Discovery
/// without dialect metadata follows the `/v1` Chat Completions convention.
pub fn route(model: &worktable_events::ModelInfo, services: Services) -> Option<Route> {
    let service = SERVICES
        .into_iter()
        .find(|service| model.group.as_deref() == Some(service.name()))?;
    if !services.enabled(service) {
        return None;
    }
    let dialect = match model.api.as_deref() {
        Some("responses" | "openai") => Dialect::Responses,
        Some("messages" | "anthropic") => Dialect::Messages,
        Some("chat" | "openai-compatible" | "chat-completions") | None => Dialect::Chat,
        Some(_) => return None,
    };
    Some(Route { service, dialect })
}

fn check_api_key(api_key: &str) -> anyhow::Result<()> {
    if api_key.trim().is_empty() {
        return Err(anyhow!("OpenCode needs an API key"));
    }
    Ok(())
}

fn session_headers(session: &str) -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        SESSION_HEADER,
        HeaderValue::from_str(session)
            .map_err(|_| anyhow!("OpenCode session id is not a valid header value"))?,
    );
    Ok(headers)
}

pub fn chat_client(
    service: Service,
    api_key: &str,
    session: &str,
) -> anyhow::Result<openai::CompletionsClient> {
    check_api_key(api_key)?;
    openai::CompletionsClient::builder()
        .api_key(api_key)
        .base_url(service.base_url())
        .http_headers(session_headers(session)?)
        .build()
        .map_err(|error| anyhow!("failed to build the OpenCode Chat Completions client: {error}"))
}

pub fn responses_client(
    service: Service,
    api_key: &str,
    session: &str,
) -> anyhow::Result<openai::Client> {
    check_api_key(api_key)?;
    openai::Client::builder()
        .api_key(api_key)
        .base_url(service.base_url())
        .http_headers(session_headers(session)?)
        .build()
        .map_err(|error| anyhow!("failed to build the OpenCode Responses client: {error}"))
}

pub fn messages_client(
    service: Service,
    api_key: &str,
    session: &str,
) -> anyhow::Result<anthropic::Client> {
    check_api_key(api_key)?;
    anthropic::Client::builder()
        .api_key(api_key)
        .base_url(service.base_url())
        .http_headers(session_headers(session)?)
        .build()
        .map_err(|error| anyhow!("failed to build the OpenCode Messages client: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clients_reject_an_empty_key() {
        assert!(chat_client(Service::Go, "", "session").is_err());
        assert!(chat_client(Service::Zen, "   ", "session").is_err());
    }

    #[test]
    fn clients_reject_an_invalid_session_header() {
        assert!(chat_client(Service::Go, "sk-test", "bad\nsession").is_err());
    }

    #[test]
    fn clients_target_their_gateway_and_carry_the_session() {
        let session = HeaderValue::from_static("session-123");

        let client = chat_client(Service::Go, "sk-test", "session-123")
            .expect("client should build without network access");
        assert_eq!(client.base_url(), GO_BASE_URL);
        assert_eq!(client.headers().get(SESSION_HEADER), Some(&session));

        let client = chat_client(Service::Zen, "sk-test", "session-123")
            .expect("client should build without network access");
        assert_eq!(client.base_url(), ZEN_BASE_URL);

        let client = responses_client(Service::Zen, "sk-test", "session-123")
            .expect("client should build without network access");
        assert_eq!(client.base_url(), ZEN_BASE_URL);
        assert_eq!(client.headers().get(SESSION_HEADER), Some(&session));

        // The Anthropic builder strips `/v1`; the Messages path re-adds it.
        let client = messages_client(Service::Go, "sk-test", "session-123")
            .expect("client should build without network access");
        assert_eq!(client.base_url(), "https://opencode.ai/zen/go");
        assert_eq!(client.headers().get(SESSION_HEADER), Some(&session));
    }

    fn metadata(group: Option<&str>, api: Option<&str>) -> worktable_events::ModelInfo {
        worktable_events::ModelInfo {
            id: "discovered-model".into(),
            name: "Discovered model".into(),
            group: group.map(str::to_owned),
            api: api.map(str::to_owned),
        }
    }

    #[test]
    fn route_requires_enabled_endpoint_owned_service() {
        for service in SERVICES {
            let model = metadata(Some(service.name()), None);
            assert_eq!(
                route(&model, Services::default()),
                Some(Route {
                    service,
                    dialect: Dialect::Chat,
                })
            );
            assert_eq!(
                route(&model, Services::default().with(service, false)),
                None
            );
        }
        for group in [None, Some("unknown"), Some("go")] {
            assert_eq!(route(&metadata(group, None), Services::default()), None);
        }
    }

    #[test]
    fn route_uses_only_advertised_dialects() {
        for (api, dialect) in [
            (None, Dialect::Chat),
            (Some("chat"), Dialect::Chat),
            (Some("openai-compatible"), Dialect::Chat),
            (Some("chat-completions"), Dialect::Chat),
            (Some("responses"), Dialect::Responses),
            (Some("openai"), Dialect::Responses),
            (Some("messages"), Dialect::Messages),
            (Some("anthropic"), Dialect::Messages),
        ] {
            assert_eq!(
                route(
                    &metadata(Some(Service::Zen.name()), api),
                    Services::default()
                ),
                Some(Route {
                    service: Service::Zen,
                    dialect,
                })
            );
        }
        for api in ["unknown", ""] {
            assert_eq!(
                route(
                    &metadata(Some(Service::Go.name()), Some(api)),
                    Services::default()
                ),
                None
            );
        }
    }

    #[test]
    fn service_flags_round_trip_through_the_store() {
        let store = SqliteStore::connect(":memory:").expect("connect");
        store.migrate().expect("migrate");
        assert_eq!(Services::from_store(&store), Services::default());

        Services {
            go: false,
            zen: true,
        }
        .write_to_store(&store)
        .expect("write");
        assert_eq!(
            Services::from_store(&store),
            Services {
                go: false,
                zen: true
            }
        );
        assert_eq!(Service::from_id("zen"), Some(Service::Zen));
        assert_eq!(Service::from_id("go"), Some(Service::Go));
        assert_eq!(Service::from_id("nope"), None);
    }
}
