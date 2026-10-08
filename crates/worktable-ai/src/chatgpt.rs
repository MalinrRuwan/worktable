//! Worktable-owned ChatGPT subscription authentication.
//!
//! Rig 0.42 keeps its Authenticator private. Its public `authorize` method uses
//! that authenticator and device-code handler; we bridge its native record via
//! a private, disposable file, never the user's shared Codex auth file.

use rig::providers::chatgpt::{ChatGPTAuth, Client};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
#[cfg(any(not(target_arch = "wasm32"), test))]
use worktable_db::ProviderCredential;
use worktable_db::SqliteStore;

pub const ID: &str = "chatgpt-subscription";
pub const NAME: &str = "ChatGPT subscription";
pub const API_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const SIGN_IN_AGAIN: &str =
    "ChatGPT subscription needs authorization. Sign in again in Settings → Providers.";

// Only held around DB reads/writes, never across provider I/O. Logout and
// successful login use this same guard so refresh cannot resurrect credentials.
pub(crate) static CREDENTIAL_LOCK: Mutex<()> = Mutex::new(());
#[cfg(not(target_arch = "wasm32"))]
static REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A refreshed bearer token and the account header for subscription requests.
/// Deliberately redacted: Rig's private AuthContext derives an unsafe Debug.
#[derive(Clone)]
pub struct AuthContext {
    pub access_token: String,
    pub account_id: Option<String>,
}

impl std::fmt::Debug for AuthContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthContext(<redacted>)")
    }
}

// JSON-compatible with the pinned Rig native AuthRecord. Never derive Debug.
#[derive(Default, Serialize, Deserialize)]
struct AuthRecord {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_at: Option<i64>,
    account_id: Option<String>,
}

fn parse_record(json: &str) -> Result<AuthRecord, String> {
    serde_json::from_str(json).map_err(|_| SIGN_IN_AGAIN.to_owned())
}

fn context(record: AuthRecord) -> Result<AuthContext, String> {
    let access_token = record
        .access_token
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| SIGN_IN_AGAIN.to_owned())?;
    Ok(AuthContext {
        access_token,
        account_id: record.account_id,
    })
}

/// Build a Responses-dialect client from an already authorized record.
/// AccessToken mode never reads a shared auth file or starts device login.
pub(crate) fn client_from_record(json: &str) -> Result<Client, String> {
    client_from_context(context(parse_record(json)?)?)
}

fn client_from_context(auth: AuthContext) -> Result<Client, String> {
    Client::builder()
        .api_key(ChatGPTAuth::AccessToken {
            access_token: auth.access_token,
            account_id: auth.account_id,
        })
        .allow_device_flow(false)
        .on_device_code(|_| {})
        .build()
        .map_err(|_| "Could not build the ChatGPT subscription client.".to_owned())
}

impl AuthContext {
    pub(crate) fn record_json(&self) -> String {
        serde_json::json!({
            "access_token": self.access_token,
            "account_id": self.account_id,
        })
        .to_string()
    }
}

#[cfg(not(target_arch = "wasm32"))]
struct EphemeralRecord(tempfile::NamedTempFile);

#[cfg(not(target_arch = "wasm32"))]
impl EphemeralRecord {
    fn new(json: &str) -> Result<Self, String> {
        use std::io::Write as _;
        // NamedTempFile is exclusive and mode 0600 on Unix. On macOS/Unix,
        // ignore TMPDIR so an override cannot put tokens in repo/scratch.
        #[cfg(unix)]
        let directory = std::path::PathBuf::from("/tmp");
        #[cfg(not(unix))]
        let directory = std::env::temp_dir();
        let mut file = tempfile::Builder::new()
            .prefix("worktable-chatgpt-")
            .tempfile_in(directory)
            .map_err(|_| "Could not prepare private ChatGPT authorization storage.".to_owned())?;
        file.write_all(json.as_bytes())
            .map_err(|_| "Could not prepare private ChatGPT authorization storage.".to_owned())?;
        Ok(Self(file))
    }

    fn read(&self) -> Result<String, String> {
        let json = std::fs::read_to_string(self.0.path()).map_err(|_| SIGN_IN_AGAIN.to_owned())?;
        context(parse_record(&json)?)?;
        Ok(json)
    }
}

/// Refresh without interactive fallback, returning headers for model discovery.
/// Errors intentionally discard Rig's text: failed OAuth responses may contain
/// raw token bodies. A changed/deleted DB record invalidates a late refresh.
#[cfg(not(target_arch = "wasm32"))]
pub async fn auth_context(store: &SqliteStore) -> Result<AuthContext, String> {
    let _refresh = REFRESH_LOCK.lock().await;
    let original = {
        let _guard = CREDENTIAL_LOCK.lock().unwrap();
        store
            .read_provider_credential(ID)
            .map_err(|_| SIGN_IN_AGAIN.to_owned())?
            .filter(|credential| credential.kind == "oauth")
            .ok_or_else(|| SIGN_IN_AGAIN.to_owned())?
    };
    parse_record(&original.key)?;
    let file = EphemeralRecord::new(&original.key)?;
    let client = Client::builder()
        .oauth()
        .auth_file(file.0.path())
        .allow_device_flow(false)
        .on_device_code(|_| {})
        .build()
        .map_err(|_| SIGN_IN_AGAIN.to_owned())?;
    client
        .authorize()
        .await
        .map_err(|_| SIGN_IN_AGAIN.to_owned())?;
    let json = file.read()?;
    let auth = context(parse_record(&json)?)?;
    let _guard = CREDENTIAL_LOCK.lock().unwrap();
    let current = store
        .read_provider_credential(ID)
        .map_err(|_| SIGN_IN_AGAIN.to_owned())?;
    if !same_credential(current.as_ref(), &original) {
        return Err(SIGN_IN_AGAIN.to_owned());
    }
    store
        .write_provider_credential(
            ID,
            &ProviderCredential {
                kind: "oauth".to_owned(),
                key: json,
            },
        )
        .map_err(|_| {
            "Could not save ChatGPT authorization. Sign in again in Settings.".to_owned()
        })?;
    Ok(auth)
}

#[cfg(any(not(target_arch = "wasm32"), test))]
fn same_credential(current: Option<&ProviderCredential>, original: &ProviderCredential) -> bool {
    current.is_some_and(|current| current.kind == original.kind && current.key == original.key)
}

#[cfg(target_arch = "wasm32")]
pub async fn auth_context(_store: &SqliteStore) -> Result<AuthContext, String> {
    Err("ChatGPT subscription sign-in is available in the desktop app.".to_owned())
}

/// Non-interactive refresh plus a Responses client with the account ID header.
pub async fn authorized_client(store: &SqliteStore) -> Result<Client, String> {
    client_from_context(auth_context(store).await?)
}

/// Only called in response to explicit Sign in. The runtime owns cancellation,
/// session identity and the successful DB commit; dropping this future removes
/// the disposable file and drops Rig's in-flight request/polling loop.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn login(
    on_code: impl Fn(String, String) + Send + Sync + 'static,
) -> Result<String, String> {
    let file = EphemeralRecord::new("{}")?;
    let client = Client::builder()
        .oauth()
        .auth_file(file.0.path())
        .allow_device_flow(true)
        .on_device_code(move |prompt| {
            // Rig supplies a constant public verification URL. Never forward
            // token-bearing URLs or a response body from the provider.
            if prompt.verification_uri == "https://auth.openai.com/codex/device" {
                on_code(prompt.user_code, prompt.verification_uri);
            }
        })
        .build()
        .map_err(|_| "Could not start ChatGPT sign-in. Try again in Settings.".to_owned())?;
    client.authorize().await.map_err(|_| {
        "ChatGPT sign-in did not complete. Try signing in again in Settings.".to_owned()
    })?;
    file.read()
}

#[cfg(target_arch = "wasm32")]
pub(crate) async fn login(
    _on_code: impl Fn(String, String) + Send + Sync + 'static,
) -> Result<String, String> {
    Err("ChatGPT subscription sign-in is available in the desktop app.".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_record_json_and_client_are_compatible_and_redacted() {
        let json = r#"{"access_token":"fake-access","refresh_token":"fake-refresh","id_token":"fake-id","expires_at":4102444800,"account_id":"test-account"}"#;
        let record = parse_record(json).unwrap();
        let roundtrip = serde_json::to_string(&record).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&roundtrip).unwrap(),
            serde_json::from_str::<serde_json::Value>(json).unwrap()
        );
        let auth = context(record).unwrap();
        assert_eq!(auth.account_id.as_deref(), Some("test-account"));
        assert!(!format!("{auth:?}").contains("fake-access"));
        let client = client_from_record(json).unwrap();
        assert!(!format!("{client:?}").contains("fake-access"));
    }

    #[test]
    fn malformed_credentials_have_actionable_redacted_errors() {
        for json in ["not-json fake-secret", "{}", r#"{"access_token":""}"#] {
            assert_eq!(client_from_record(json).unwrap_err(), SIGN_IN_AGAIN);
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn missing_or_invalid_oauth_fails_before_file_or_provider_io() {
        let store = SqliteStore::connect(":memory:").unwrap();
        store.migrate().unwrap();
        assert_eq!(
            futures::executor::block_on(auth_context(&store)).unwrap_err(),
            SIGN_IN_AGAIN
        );
        for (kind, key) in [
            ("api_key", "fake-secret"),
            ("oauth", "invalid JSON fake-secret"),
        ] {
            store
                .write_provider_credential(
                    ID,
                    &ProviderCredential {
                        kind: kind.to_owned(),
                        key: key.to_owned(),
                    },
                )
                .unwrap();
            assert_eq!(
                futures::executor::block_on(auth_context(&store)).unwrap_err(),
                SIGN_IN_AGAIN
            );
            assert_eq!(
                store.read_provider_credential(ID).unwrap().unwrap().key,
                key,
                "failed authorization must not change the stored credential"
            );
        }
    }

    #[test]
    fn late_refresh_cannot_replace_logout_or_new_login() {
        let original = ProviderCredential {
            kind: "oauth".to_owned(),
            key: "original".to_owned(),
        };
        let replacement = ProviderCredential {
            kind: "oauth".to_owned(),
            key: "replacement".to_owned(),
        };
        assert!(same_credential(Some(&original), &original));
        assert!(!same_credential(None, &original));
        assert!(!same_credential(Some(&replacement), &original));
    }
}
