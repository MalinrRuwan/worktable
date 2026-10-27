//! Typed local control protocol shared by the Worktable app and `worktable-cli`.
//!
//! The protocol is one JSON object per line over a Unix socket. The app
//! listens only when `WORKTABLE_CONTROL_SOCKET` is set, binds that path with
//! owner-only permissions, and serves one request per connection. This crate
//! deliberately depends on neither GPUI nor the app: it is the contract both
//! sides compile against, plus the client used by tools and tests.

use std::fmt;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Environment variable that enables the app's control server.
pub const SOCKET_ENV: &str = "WORKTABLE_CONTROL_SOCKET";

/// Default time a client waits for a reply.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Read the configured control socket path, if any.
pub fn socket_from_env() -> Option<PathBuf> {
    let path = std::env::var_os(SOCKET_ENV)?;
    if path.is_empty() {
        return None;
    }
    Some(PathBuf::from(path))
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// A top-level user-facing page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Page {
    Entries,
    Assistant,
    Settings,
}

/// One Settings category page. `Settings { tab: None }` shows the category
/// list instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SettingsTab {
    General,
    Appearance,
    Data,
    Providers,
}

/// Entry ordering, matching the library menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SortMode {
    Time,
    Alpha,
    Topic,
}

/// Theme preference; `system` follows the window appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeMode {
    Light,
    Dark,
    System,
}

/// Entry presentation label. `None` clears the label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LabelColor {
    Red,
    Amber,
    Green,
    Blue,
    Purple,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OnboardingAction {
    Start,
    Next,
    Back,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WindowAction {
    Show,
    Hide,
    Focus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AssistantAction {
    Ask,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KnowledgeAction {
    Build,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatsAction {
    Open,
    New,
    Close,
}

/// Every action the local control socket accepts.
///
/// Serialized as an internally tagged object with an `action` key in
/// kebab-case, for example:
///
/// ```json
/// {"action":"add-note","content":"Buy milk"}
/// {"action":"assistant","intent":"ask","text":"What did I save?"}
/// {"action":"label","color":"amber"}
/// ```
///
/// Nested action enums use the `intent` field so the top-level `action` tag
/// stays unambiguous.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Command {
    /// Liveness probe; the app answers as soon as the window is ready.
    Ping,
    /// Full state snapshot (see the crate docs and `docs/cli.md`).
    State,
    Page {
        page: Page,
    },
    Settings {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab: Option<SettingsTab>,
    },
    /// Close one overlay / page level: entry detail, chats, onboarding back,
    /// settings page, then the assistant page.
    Back,
    AddNote {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Import a local image through the app's media library.
    AddImage {
        path: String,
    },
    Search {
        query: String,
    },
    ClearSearch,
    Select {
        id: String,
        #[serde(default)]
        extend: bool,
    },
    /// Open the entry with `id`, or the current selection when omitted.
    Open {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// Full, uncapped entry detail (the state snapshot caps long content).
    Entry {
        id: String,
    },
    /// Dismiss the entry detail, otherwise the chats sheet.
    Close,
    /// Replace the open entry's markdown body.
    Edit {
        content: String,
    },
    /// Delete `id`, or the current selection when omitted.
    Delete {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// Set or clear (`none`) an entry label.
    Label {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        color: LabelColor,
    },
    /// Copy an entry's `title\ncontent`, or the current selection as a list.
    Copy {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// Set the sort mode; `ascending` overrides the mode's default direction.
    Sort {
        mode: SortMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ascending: Option<bool>,
    },
    Theme {
        mode: ThemeMode,
    },
    ToggleTheme,
    /// Keep running in the menu bar when the window closes.
    Background {
        enabled: bool,
    },
    Onboarding {
        intent: OnboardingAction,
    },
    Assistant {
        intent: AssistantAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    Knowledge {
        intent: KnowledgeAction,
    },
    Chats {
        intent: ChatsAction,
    },
    /// Window visibility is owned by the app shell, not the view.
    Window {
        intent: WindowAction,
    },
    Quit,
}

impl Command {
    /// The wire-format action name (`add-note`, `toggle-theme`, ...).
    pub fn action(&self) -> &'static str {
        match self {
            Command::Ping => "ping",
            Command::State => "state",
            Command::Page { .. } => "page",
            Command::Settings { .. } => "settings",
            Command::Back => "back",
            Command::AddNote { .. } => "add-note",
            Command::AddImage { .. } => "add-image",
            Command::Search { .. } => "search",
            Command::ClearSearch => "clear-search",
            Command::Select { .. } => "select",
            Command::Open { .. } => "open",
            Command::Entry { .. } => "entry",
            Command::Close => "close",
            Command::Edit { .. } => "edit",
            Command::Delete { .. } => "delete",
            Command::Label { .. } => "label",
            Command::Copy { .. } => "copy",
            Command::Sort { .. } => "sort",
            Command::Theme { .. } => "theme",
            Command::ToggleTheme => "toggle-theme",
            Command::Background { .. } => "background",
            Command::Onboarding { .. } => "onboarding",
            Command::Assistant { .. } => "assistant",
            Command::Knowledge { .. } => "knowledge",
            Command::Chats { .. } => "chats",
            Command::Window { .. } => "window",
            Command::Quit => "quit",
        }
    }
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// One reply line from the app.
///
/// `state` is always a JSON value: the full app snapshot for most commands,
/// a command-specific object where noted in `docs/cli.md` (copy text, full
/// entry, window bounds), and `null` for failures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub state: Value,
}

impl Response {
    pub fn ok(state: Value) -> Self {
        Self {
            ok: true,
            error: None,
            message: None,
            state,
        }
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            message: None,
            state: Value::Null,
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Errors from one local control round trip.
#[derive(Debug)]
pub enum ClientError {
    /// The socket could not be reached (stale path, app not running).
    Connect(std::io::Error),
    /// Writing the request or reading the reply failed.
    Io(std::io::Error),
    /// The request could not be encoded.
    Encode(serde_json::Error),
    /// The app's reply was not a valid [`Response`].
    Decode(serde_json::Error),
    /// The peer closed the connection without a reply line.
    EmptyReply,
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Connect(error) => {
                write!(
                    formatter,
                    "could not connect to the control socket: {error}"
                )
            }
            ClientError::Io(error) => write!(formatter, "control socket I/O failed: {error}"),
            ClientError::Encode(error) => {
                write!(formatter, "could not encode the command: {error}")
            }
            ClientError::Decode(error) => write!(formatter, "invalid control reply: {error}"),
            ClientError::EmptyReply => {
                write!(
                    formatter,
                    "the app closed the control connection without a reply"
                )
            }
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ClientError::Connect(error) | ClientError::Io(error) => Some(error),
            ClientError::Encode(error) | ClientError::Decode(error) => Some(error),
            ClientError::EmptyReply => None,
        }
    }
}

/// A one-request-per-connection Unix-socket client.
///
/// ```no_run
/// use worktable_control::{Client, Command};
///
/// let client = Client::new("/tmp/worktable.sock");
/// let response = client.send(&Command::Ping)?;
/// assert!(response.ok);
/// # Ok::<(), worktable_control::ClientError>(())
/// ```
#[derive(Debug, Clone)]
pub struct Client {
    path: PathBuf,
    timeout: Duration,
}

impl Client {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Override the read/write timeout for slow or wedged peers.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Send one command and return the parsed reply.
    pub fn send(&self, command: &Command) -> Result<Response, ClientError> {
        let mut stream = UnixStream::connect(&self.path).map_err(ClientError::Connect)?;
        stream
            .set_write_timeout(Some(self.timeout))
            .map_err(ClientError::Io)?;
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(ClientError::Io)?;

        let mut payload = serde_json::to_vec(command).map_err(ClientError::Encode)?;
        payload.push(b'\n');
        stream.write_all(&payload).map_err(ClientError::Io)?;
        stream.flush().map_err(ClientError::Io)?;

        let mut line = String::new();
        let mut reader = BufReader::new(stream);
        reader.read_line(&mut line).map_err(ClientError::Io)?;
        if line.trim().is_empty() {
            return Err(ClientError::EmptyReply);
        }
        serde_json::from_str(line.trim()).map_err(ClientError::Decode)
    }

    /// True when the app answers a [`Command::Ping`].
    pub fn ping(&self) -> bool {
        self.send(&Command::Ping).is_ok_and(|response| response.ok)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;
    use std::os::unix::net::UnixListener;

    fn temp_socket(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "worktable-control-test-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("control.sock");
        let _ = std::fs::remove_file(&path);
        path
    }

    fn round_trip(command: &Command) -> Command {
        let json = serde_json::to_string(command).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn commands_use_action_tag_and_kebab_case() {
        let cases = [
            (Command::Ping, r#"{"action":"ping"}"#),
            (Command::State, r#"{"action":"state"}"#),
            (
                Command::Page {
                    page: Page::Assistant,
                },
                r#"{"action":"page","page":"assistant"}"#,
            ),
            (Command::Settings { tab: None }, r#"{"action":"settings"}"#),
            (
                Command::Settings {
                    tab: Some(SettingsTab::Providers),
                },
                r#"{"action":"settings","tab":"providers"}"#,
            ),
            (Command::Back, r#"{"action":"back"}"#),
            (
                Command::AddNote {
                    content: "hello".into(),
                    title: None,
                },
                r#"{"action":"add-note","content":"hello"}"#,
            ),
            (
                Command::AddNote {
                    content: "hello".into(),
                    title: Some("Greeting".into()),
                },
                r#"{"action":"add-note","content":"hello","title":"Greeting"}"#,
            ),
            (
                Command::AddImage {
                    path: "/tmp/a.png".into(),
                },
                r#"{"action":"add-image","path":"/tmp/a.png"}"#,
            ),
            (
                Command::Search {
                    query: "rust".into(),
                },
                r#"{"action":"search","query":"rust"}"#,
            ),
            (Command::ClearSearch, r#"{"action":"clear-search"}"#),
            (
                Command::Select {
                    id: "e1".into(),
                    extend: false,
                },
                r#"{"action":"select","id":"e1","extend":false}"#,
            ),
            (Command::Open { id: None }, r#"{"action":"open"}"#),
            (
                Command::Open {
                    id: Some("e1".into()),
                },
                r#"{"action":"open","id":"e1"}"#,
            ),
            (
                Command::Entry { id: "e1".into() },
                r#"{"action":"entry","id":"e1"}"#,
            ),
            (Command::Close, r#"{"action":"close"}"#),
            (
                Command::Edit {
                    content: "new body".into(),
                },
                r#"{"action":"edit","content":"new body"}"#,
            ),
            (Command::Delete { id: None }, r#"{"action":"delete"}"#),
            (
                Command::Label {
                    id: None,
                    color: LabelColor::Amber,
                },
                r#"{"action":"label","color":"amber"}"#,
            ),
            (
                Command::Label {
                    id: None,
                    color: LabelColor::None,
                },
                r#"{"action":"label","color":"none"}"#,
            ),
            (Command::Copy { id: None }, r#"{"action":"copy"}"#),
            (
                Command::Sort {
                    mode: SortMode::Alpha,
                    ascending: Some(true),
                },
                r#"{"action":"sort","mode":"alpha","ascending":true}"#,
            ),
            (
                Command::Theme {
                    mode: ThemeMode::Dark,
                },
                r#"{"action":"theme","mode":"dark"}"#,
            ),
            (Command::ToggleTheme, r#"{"action":"toggle-theme"}"#),
            (
                Command::Background { enabled: false },
                r#"{"action":"background","enabled":false}"#,
            ),
            (
                Command::Onboarding {
                    intent: OnboardingAction::Skip,
                },
                r#"{"action":"onboarding","intent":"skip"}"#,
            ),
            (
                Command::Assistant {
                    intent: AssistantAction::Ask,
                    text: Some("hi".into()),
                },
                r#"{"action":"assistant","intent":"ask","text":"hi"}"#,
            ),
            (
                Command::Assistant {
                    intent: AssistantAction::Cancel,
                    text: None,
                },
                r#"{"action":"assistant","intent":"cancel"}"#,
            ),
            (
                Command::Knowledge {
                    intent: KnowledgeAction::Build,
                },
                r#"{"action":"knowledge","intent":"build"}"#,
            ),
            (
                Command::Chats {
                    intent: ChatsAction::New,
                },
                r#"{"action":"chats","intent":"new"}"#,
            ),
            (
                Command::Window {
                    intent: WindowAction::Focus,
                },
                r#"{"action":"window","intent":"focus"}"#,
            ),
            (Command::Quit, r#"{"action":"quit"}"#),
        ];
        for (command, expected) in cases {
            assert_eq!(serde_json::to_string(&command).unwrap(), expected);
            assert_eq!(round_trip(&command), command);
        }
    }

    #[test]
    fn nested_actions_round_trip_every_variant() {
        let commands = [
            Command::Onboarding {
                intent: OnboardingAction::Start,
            },
            Command::Onboarding {
                intent: OnboardingAction::Next,
            },
            Command::Onboarding {
                intent: OnboardingAction::Back,
            },
            Command::Knowledge {
                intent: KnowledgeAction::Cancel,
            },
            Command::Chats {
                intent: ChatsAction::Open,
            },
            Command::Chats {
                intent: ChatsAction::Close,
            },
            Command::Window {
                intent: WindowAction::Show,
            },
            Command::Window {
                intent: WindowAction::Hide,
            },
        ];
        for command in commands {
            assert_eq!(round_trip(&command), command);
        }
    }

    #[test]
    fn response_omits_absent_fields_and_keeps_state() {
        let response = Response::ok(serde_json::json!({"mode": "entries"}));
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["state"]["mode"], "entries");
        assert!(json.get("error").is_none());
        assert!(json.get("message").is_none());

        let response = Response::error("missing window").with_message("failed");
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"], "missing window");
        assert_eq!(json["message"], "failed");
        assert!(json["state"].is_null());
    }

    #[test]
    fn response_round_trips() {
        let response =
            Response::ok(serde_json::json!({"entries": []})).with_message("State loaded");
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), response);
    }

    #[test]
    fn client_sends_one_line_and_reads_one_reply() {
        let path = temp_socket("round-trip");
        let listener = UnixListener::bind(&path).unwrap();
        let server_path = path.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line.trim(), r#"{"action":"page","page":"entries"}"#);
            let response =
                Response::ok(serde_json::json!({"mode": "entries"})).with_message("Page shown");
            serde_json::to_writer(&mut stream, &response).unwrap();
            stream.write_all(b"\n").unwrap();
            let _ = std::fs::remove_file(server_path);
        });

        let client = Client::new(&path).with_timeout(Duration::from_secs(2));
        let response = client
            .send(&Command::Page {
                page: Page::Entries,
            })
            .unwrap();
        assert!(response.ok);
        assert_eq!(response.message.as_deref(), Some("Page shown"));
        assert_eq!(response.state["mode"], "entries");
        server.join().unwrap();
    }

    #[test]
    fn client_reports_a_missing_socket_as_a_connect_error() {
        let path = temp_socket("missing");
        let client = Client::new(&path).with_timeout(Duration::from_millis(200));
        let error = client.send(&Command::Ping).unwrap_err();
        assert!(matches!(error, ClientError::Connect(_)), "{error:?}");
        assert!(!client.ping());
    }

    #[test]
    fn client_reports_an_empty_reply() {
        let path = temp_socket("empty");
        let listener = UnixListener::bind(&path).unwrap();
        let server_path = path.clone();
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            let _ = std::fs::remove_file(server_path);
        });
        let client = Client::new(&path).with_timeout(Duration::from_secs(2));
        let error = client.send(&Command::Ping).unwrap_err();
        assert!(matches!(error, ClientError::EmptyReply), "{error:?}");
        server.join().unwrap();
    }

    #[test]
    fn client_reports_invalid_json_replies() {
        let path = temp_socket("invalid");
        let listener = UnixListener::bind(&path).unwrap();
        let server_path = path.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            stream.write_all(b"not json\n").unwrap();
            let _ = std::fs::remove_file(server_path);
        });
        let client = Client::new(&path).with_timeout(Duration::from_secs(2));
        let error = client.send(&Command::Ping).unwrap_err();
        assert!(matches!(error, ClientError::Decode(_)), "{error:?}");
        server.join().unwrap();
    }

    #[test]
    fn socket_from_env_reads_the_documented_variable() {
        // The test binary shares the process environment, so restore it after.
        let previous = std::env::var_os(SOCKET_ENV);
        unsafe {
            std::env::set_var(SOCKET_ENV, "/tmp/example.sock");
        }
        assert_eq!(socket_from_env(), Some(PathBuf::from("/tmp/example.sock")));
        unsafe {
            std::env::remove_var(SOCKET_ENV);
        }
        assert_eq!(socket_from_env(), None);
        if let Some(previous) = previous {
            unsafe {
                std::env::set_var(SOCKET_ENV, previous);
            }
        }
    }
}
