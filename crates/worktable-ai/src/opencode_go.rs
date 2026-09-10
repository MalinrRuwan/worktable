//! The `opencode-go` provider — Worktable's custom provider for
//! [OpenCode Go](https://opencode.ai/docs/go/).
//!
//! OpenCode Go is a subscription gateway for open coding models. Its primary
//! API is OpenAI-compatible Chat Completions, served from
//! `https://opencode.ai/zen/go/v1`, so this provider follows rig's documented
//! path for OpenAI-compatible endpoints: an `openai::CompletionsClient`
//! pointed at the Go base URL
//! ([Providers & Clients](https://rig.rs/docs/concepts/provider_clients)).
//!
//! Some Go models are only served through other API dialects (OpenAI
//! Responses for `grok-*`, `gpt-*`, and `muse-spark-*`; Anthropic Messages
//! for `minimax-*` and `qwen*`). Those are intentionally not listed in
//! [`MODELS`] yet: selecting one would fail at the first request instead of
//! working. Adding them means routing per model, which the provider module is
//! shaped to grow into — [`client`] is the single construction point.
//!
//! Authentication is a bearer API key issued by OpenCode Zen; the app stores
//! it in its own database, so `OPENCODE_GO_API_KEY` is only used by
//! `ProviderClient::from_env`, which Worktable does not rely on.

use anyhow::anyhow;
use rig::http_client::{HeaderMap, HeaderValue};
use rig::providers::openai;

/// Stable provider id used in the database, UI, and worker protocol.
pub const ID: &str = "opencode-go";
/// Display name shown in Settings.
pub const NAME: &str = "OpenCode Go";
/// OpenAI-compatible endpoint root for the Go subscription.
pub const BASE_URL: &str = "https://opencode.ai/zen/go/v1";
/// Environment variable read by [`openai::CompletionsClient`]'s
/// `ProviderClient::from_env`; the app normally passes the stored key instead.
pub const API_KEY_ENV: &str = "OPENCODE_GO_API_KEY";

/// Header the Go gateway uses to route a conversation efficiently. OpenCode's
/// docs ask every client to send a stable session id per conversation
/// (<https://opencode.ai/docs/go/#where-can-i-use-it>); omitting it makes the
/// gateway reject requests with `MissingSessionID`.
pub const SESSION_HEADER: &str = "x-opencode-session";

/// The rig client this provider talks through.
pub type Client = openai::CompletionsClient;

/// Build a client for the Go gateway with an explicit API key.
///
/// `session` is the stable per-conversation id sent in [`SESSION_HEADER`].
/// This is the one place that knows the gateway URL; everything else in the
/// app builds agents from the returned client.
pub fn client(api_key: &str, session: &str) -> anyhow::Result<Client> {
    if api_key.trim().is_empty() {
        return Err(anyhow!("OpenCode Go needs an API key"));
    }
    let mut headers = HeaderMap::new();
    // `&'static str` implements `IntoHeaderName`; the value still needs
    // validation because a session id could contain a newline.
    headers.insert(
        SESSION_HEADER,
        HeaderValue::from_str(session)
            .map_err(|_| anyhow!("OpenCode Go session id is not a valid header value"))?,
    );
    Client::builder()
        .api_key(api_key)
        .base_url(BASE_URL)
        .http_headers(headers)
        .build()
        .map_err(|error| anyhow!("failed to build the OpenCode Go client: {error}"))
}

/// Models served through the Go Chat Completions endpoint, newest first.
///
/// The list mirrors the official endpoint table
/// (<https://opencode.ai/docs/go/#endpoints>); model ids are passed straight
/// through to the gateway.
pub const MODELS: &[(&str, &str)] = &[
    ("glm-5.3", "GLM-5.3"),
    ("glm-5.3-flash", "GLM-5.3 Flash"),
    ("glm-5.2", "GLM-5.2"),
    ("glm-5.1", "GLM-5.1"),
    ("kimi-k3", "Kimi K3"),
    ("kimi-k2.7-code", "Kimi K2.7 Code"),
    ("kimi-k2.6", "Kimi K2.6"),
    ("deepseek-v4-pro", "DeepSeek V4 Pro"),
    ("deepseek-v4-flash", "DeepSeek V4 Flash"),
    ("deepseek-flash", "DeepSeek V4.1 Flash"),
    (
        "deepseek-v4-flash-vision-exp",
        "DeepSeek V4 Flash Vision Exp",
    ),
    ("mimo-v2.5", "MiMo-V2.5"),
    ("mimo-v2.5-pro", "MiMo-V2.5 Pro"),
    ("longcat-2.0", "LongCat-2.0"),
    ("hy4-preview", "Hy4 preview"),
    ("hy3", "Hy3"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_rejects_an_empty_key() {
        assert!(client("", "session").is_err());
        assert!(client("   ", "session").is_err());
    }

    #[test]
    fn client_rejects_an_invalid_session_header() {
        assert!(client("sk-test", "bad\nsession").is_err());
    }

    #[test]
    fn client_targets_the_go_gateway_and_carries_the_session() {
        let client =
            client("sk-test", "session-123").expect("client should build without network access");
        // The base URL and the routing session are the two things this module
        // owns; assert both reached the client rather than a default.
        assert_eq!(client.base_url(), BASE_URL);
        assert_eq!(
            client.headers().get(SESSION_HEADER),
            Some(&HeaderValue::from_static("session-123")),
            "the Go gateway rejects requests without {SESSION_HEADER}"
        );
    }

    #[test]
    fn catalog_ids_are_unique_and_named() {
        let mut ids: Vec<&str> = MODELS.iter().map(|(id, _)| *id).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before, "model ids must be unique");
        for (id, name) in MODELS {
            assert!(!id.is_empty());
            assert!(!name.is_empty());
        }
    }
}
