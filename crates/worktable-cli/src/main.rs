//! `worktable-cli` — drive the real Worktable app through its local control
//! socket.
//!
//! The CLI never opens the database or keeps a second copy of app state. It
//! launches the real app for `start`, sends typed JSON commands over a Unix
//! socket, and prints the app's JSON replies. See `docs/cli.md`.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::json;
use worktable_control::{
    AssistantAction, ChatsAction, Client, Command, KnowledgeAction, LabelColor, OnboardingAction,
    Page, Response, SettingsTab, SortMode, ThemeMode, WindowAction, socket_from_env,
};

const START_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(CliError::Response(response)) => {
            eprintln!("{}", pretty(&*response));
            std::process::ExitCode::FAILURE
        }
        Err(CliError::Message(message)) => {
            eprintln!("{}", pretty(&json!({ "ok": false, "error": message })));
            std::process::ExitCode::FAILURE
        }
    }
}

#[derive(Debug)]
enum CliError {
    Message(String),
    Response(Box<Response>),
}

impl From<String> for CliError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

impl From<&str> for CliError {
    fn from(message: &str) -> Self {
        Self::Message(message.to_owned())
    }
}

impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        Self::Message(error.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(error: serde_json::Error) -> Self {
        Self::Message(error.to_string())
    }
}

type CliResult<T> = Result<T, CliError>;

// ---------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "worktable-cli",
    version,
    about = "Drive the real Worktable app through its local control socket",
    after_help = "Examples:\n  worktable-cli start\n  worktable-cli add-note \"Buy milk\"\n  worktable-cli state\n  worktable-cli screenshot\n  worktable-cli stop"
)]
struct Cli {
    /// Control socket path (default: WORKTABLE_CONTROL_SOCKET).
    #[arg(
        long,
        global = true,
        env = "WORKTABLE_CONTROL_SOCKET",
        value_name = "PATH"
    )]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Launch the real app with an isolated data directory and wait for Ping.
    Start(StartArgs),
    /// Ask the app to quit and wait for its socket and process to finish.
    Stop,
    /// Print the full state snapshot.
    State,
    /// Focus the window, capture its bounds, and write a PNG screenshot.
    Screenshot(ScreenshotArgs),
    /// Send a JSON array of protocol commands sequentially.
    Batch {
        /// File containing the JSON array.
        file: PathBuf,
    },
    /// Send one raw protocol command object.
    Cmd {
        /// JSON command, for example '{"action":"ping"}'.
        json: String,
    },
    /// Show Entries, Assistant, or Settings.
    Page {
        #[arg(value_enum)]
        page: PageArg,
    },
    /// Open Settings, optionally on one category page.
    Settings {
        #[arg(value_enum)]
        tab: Option<SettingsTabArg>,
    },
    /// Close the entry view, chats, a settings page, or return from Assistant.
    Back,
    /// Add a note.
    AddNote {
        content: String,
        #[arg(long)]
        title: Option<String>,
    },
    /// Import a local image as an entry.
    AddImage { path: PathBuf },
    /// Filter the entry list.
    Search { query: String },
    /// Clear the entry filter.
    ClearSearch,
    /// Select one entry.
    Select {
        id: String,
        #[arg(long)]
        extend: bool,
    },
    /// Open an entry's full content (defaults to the selection).
    Open { id: Option<String> },
    /// Print one entry's full, uncapped detail.
    Entry { id: String },
    /// Close the entry view, otherwise the chats sheet.
    Close,
    /// Replace the open entry's markdown content.
    Edit { content: String },
    /// Delete one entry (defaults to the selection).
    Delete { id: Option<String> },
    /// Set or clear an entry label.
    Label {
        #[arg(value_enum)]
        color: LabelColorArg,
        #[arg(long)]
        id: Option<String>,
    },
    /// Copy an entry (defaults to the selection) and print the copied text.
    Copy { id: Option<String> },
    /// Set the entry sort mode.
    Sort {
        #[arg(value_enum)]
        mode: SortModeArg,
        #[arg(long, conflicts_with = "descending")]
        ascending: bool,
        #[arg(long, conflicts_with = "ascending")]
        descending: bool,
    },
    /// Set the theme.
    Theme {
        #[arg(value_enum)]
        mode: ThemeModeArg,
    },
    /// Switch between light and dark.
    ToggleTheme,
    /// Keep running in the menu bar when the window closes.
    Background {
        #[arg(value_enum)]
        state: BackgroundArg,
    },
    /// Drive the first-run tour.
    Onboarding {
        #[arg(value_enum)]
        action: OnboardingArg,
    },
    /// Ask the assistant or cancel its running answer.
    Assistant {
        #[command(subcommand)]
        action: AssistantCli,
    },
    /// Build or cancel the knowledge graph.
    Knowledge {
        #[command(subcommand)]
        action: KnowledgeCli,
    },
    /// Open, start, or close the chats sheet.
    Chats {
        #[command(subcommand)]
        action: ChatsCli,
    },
    /// Show, hide, or focus the app window.
    Window {
        #[command(subcommand)]
        action: WindowCli,
    },
    /// Quit the app without waiting for it to exit.
    Quit,
}

#[derive(Debug, Args)]
struct StartArgs {
    /// Directory for the database, media, and screenshots. Defaults to a fresh
    /// temporary directory.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// JSON array of strings or {"content": "...", "title": "..."} objects to
    /// add after the app is ready.
    #[arg(long)]
    seed: Option<String>,
    /// Leave the first-run tour visible instead of skipping it.
    #[arg(long)]
    keep_onboarding: bool,
    /// App binary. Defaults to WORKTABLE_APP_BIN or the sibling worktable-app.
    #[arg(long)]
    app_bin: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ScreenshotArgs {
    /// Output PNG path. Defaults to a timestamped file in the session data dir.
    #[arg(long)]
    path: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum AssistantCli {
    /// Send a prompt.
    Ask { text: String },
    /// Cancel the running answer.
    Cancel,
}

#[derive(Debug, Subcommand)]
enum KnowledgeCli {
    Build,
    Cancel,
}

#[derive(Debug, Subcommand)]
enum ChatsCli {
    Open,
    New,
    Close,
}

#[derive(Debug, Subcommand)]
enum WindowCli {
    Show,
    Hide,
    Focus,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum PageArg {
    Entries,
    Assistant,
    Settings,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SettingsTabArg {
    General,
    Appearance,
    Data,
    Providers,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SortModeArg {
    Time,
    Alpha,
    Topic,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ThemeModeArg {
    Light,
    Dark,
    System,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LabelColorArg {
    Red,
    Amber,
    Green,
    Blue,
    Purple,
    #[value(name = "none")]
    NoLabel,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum BackgroundArg {
    On,
    Off,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OnboardingArg {
    Start,
    Next,
    Back,
    Skip,
}

impl CliCommand {
    /// Map a user-facing subcommand to the wire command. Shell-local commands
    /// (`start`, `stop`, `screenshot`, `batch`, `cmd`) return `None`.
    fn protocol_command(&self) -> Option<Command> {
        match self {
            CliCommand::Start(_)
            | CliCommand::Stop
            | CliCommand::Screenshot(_)
            | CliCommand::Batch { .. }
            | CliCommand::Cmd { .. } => None,
            CliCommand::State => Some(Command::State),
            CliCommand::Page { page } => Some(Command::Page {
                page: match page {
                    PageArg::Entries => Page::Entries,
                    PageArg::Assistant => Page::Assistant,
                    PageArg::Settings => Page::Settings,
                },
            }),
            CliCommand::Settings { tab } => Some(Command::Settings {
                tab: tab.as_ref().map(|tab| match tab {
                    SettingsTabArg::General => SettingsTab::General,
                    SettingsTabArg::Appearance => SettingsTab::Appearance,
                    SettingsTabArg::Data => SettingsTab::Data,
                    SettingsTabArg::Providers => SettingsTab::Providers,
                }),
            }),
            CliCommand::Back => Some(Command::Back),
            CliCommand::AddNote { content, title } => Some(Command::AddNote {
                content: content.clone(),
                title: title.clone(),
            }),
            CliCommand::AddImage { path } => Some(Command::AddImage {
                path: path.to_string_lossy().into_owned(),
            }),
            CliCommand::Search { query } => Some(Command::Search {
                query: query.clone(),
            }),
            CliCommand::ClearSearch => Some(Command::ClearSearch),
            CliCommand::Select { id, extend } => Some(Command::Select {
                id: id.clone(),
                extend: *extend,
            }),
            CliCommand::Open { id } => Some(Command::Open { id: id.clone() }),
            CliCommand::Entry { id } => Some(Command::Entry { id: id.clone() }),
            CliCommand::Close => Some(Command::Close),
            CliCommand::Edit { content } => Some(Command::Edit {
                content: content.clone(),
            }),
            CliCommand::Delete { id } => Some(Command::Delete { id: id.clone() }),
            CliCommand::Label { color, id } => Some(Command::Label {
                id: id.clone(),
                color: match color {
                    LabelColorArg::Red => LabelColor::Red,
                    LabelColorArg::Amber => LabelColor::Amber,
                    LabelColorArg::Green => LabelColor::Green,
                    LabelColorArg::Blue => LabelColor::Blue,
                    LabelColorArg::Purple => LabelColor::Purple,
                    LabelColorArg::NoLabel => LabelColor::None,
                },
            }),
            CliCommand::Copy { id } => Some(Command::Copy { id: id.clone() }),
            CliCommand::Sort {
                mode,
                ascending,
                descending,
            } => Some(Command::Sort {
                mode: match mode {
                    SortModeArg::Time => SortMode::Time,
                    SortModeArg::Alpha => SortMode::Alpha,
                    SortModeArg::Topic => SortMode::Topic,
                },
                ascending: if *ascending {
                    Some(true)
                } else if *descending {
                    Some(false)
                } else {
                    None
                },
            }),
            CliCommand::Theme { mode } => Some(Command::Theme {
                mode: match mode {
                    ThemeModeArg::Light => ThemeMode::Light,
                    ThemeModeArg::Dark => ThemeMode::Dark,
                    ThemeModeArg::System => ThemeMode::System,
                },
            }),
            CliCommand::ToggleTheme => Some(Command::ToggleTheme),
            CliCommand::Background { state } => Some(Command::Background {
                enabled: matches!(state, BackgroundArg::On),
            }),
            CliCommand::Onboarding { action } => Some(Command::Onboarding {
                intent: match action {
                    OnboardingArg::Start => OnboardingAction::Start,
                    OnboardingArg::Next => OnboardingAction::Next,
                    OnboardingArg::Back => OnboardingAction::Back,
                    OnboardingArg::Skip => OnboardingAction::Skip,
                },
            }),
            CliCommand::Assistant { action } => Some(Command::Assistant {
                intent: match action {
                    AssistantCli::Ask { .. } => AssistantAction::Ask,
                    AssistantCli::Cancel => AssistantAction::Cancel,
                },
                text: match action {
                    AssistantCli::Ask { text } => Some(text.clone()),
                    AssistantCli::Cancel => None,
                },
            }),
            CliCommand::Knowledge { action } => Some(Command::Knowledge {
                intent: match action {
                    KnowledgeCli::Build => KnowledgeAction::Build,
                    KnowledgeCli::Cancel => KnowledgeAction::Cancel,
                },
            }),
            CliCommand::Chats { action } => Some(Command::Chats {
                intent: match action {
                    ChatsCli::Open => ChatsAction::Open,
                    ChatsCli::New => ChatsAction::New,
                    ChatsCli::Close => ChatsAction::Close,
                },
            }),
            CliCommand::Window { action } => Some(Command::Window {
                intent: match action {
                    WindowCli::Show => WindowAction::Show,
                    WindowCli::Hide => WindowAction::Hide,
                    WindowCli::Focus => WindowAction::Focus,
                },
            }),
            CliCommand::Quit => Some(Command::Quit),
        }
    }
}

fn run() -> CliResult<()> {
    let cli = Cli::parse();
    match cli.command {
        CliCommand::Start(args) => start(cli.socket, args),
        CliCommand::Stop => stop(&require_socket(cli.socket)?),
        CliCommand::Screenshot(args) => screenshot(&require_socket(cli.socket)?, args),
        CliCommand::Batch { file } => batch(&require_socket(cli.socket)?, &file),
        CliCommand::Cmd { json: raw } => cmd(&require_socket(cli.socket)?, &raw),
        command => {
            let command = command
                .protocol_command()
                .expect("shell-local commands are handled above");
            let socket = require_socket(cli.socket)?;
            let response = require_ok(send(&socket, &command)?)?;
            let response = if matches!(command, Command::AddNote { .. }) {
                wait_for_added_note(&socket, response)?
            } else {
                response
            };
            print_command_response(&command, &response)
        }
    }
}

fn require_socket(socket: Option<PathBuf>) -> CliResult<PathBuf> {
    socket.or_else(socket_from_env).ok_or_else(|| {
        CliError::Message(
            "No control socket. Pass --socket or set WORKTABLE_CONTROL_SOCKET.".to_owned(),
        )
    })
}

fn send(socket: &Path, command: &Command) -> CliResult<Response> {
    Client::new(socket)
        .send(command)
        .map_err(|error| CliError::Message(error.to_string()))
}

fn require_ok(response: Response) -> CliResult<Response> {
    if response.ok {
        Ok(response)
    } else {
        Err(CliError::Response(Box::new(response)))
    }
}

fn pretty(value: &impl Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|error| {
        format!("{{\"ok\":false,\"error\":\"could not encode JSON: {error}\"}}")
    })
}

fn print_json(value: &impl Serialize) -> CliResult<()> {
    println!("{}", pretty(value));
    Ok(())
}

// ---------------------------------------------------------------------------
// Protocol commands
// ---------------------------------------------------------------------------

/// Print the useful slice of each reply. `state`, `entry`, and `copy` print
/// their focused value; everything else prints the full response.
fn print_command_response(command: &Command, response: &Response) -> CliResult<()> {
    match command {
        Command::State => print_json(&response.state),
        Command::Entry { .. } => print_json(&response.state["result"]["entry"]),
        Command::Copy { .. } => print_json(&response.state["result"]),
        _ => print_json(response),
    }
}

/// Add-note is queued through the app's normal insert path. Wait for the same
/// view to report the new id so the printed state already contains the note.
fn wait_for_added_note(socket: &Path, response: Response) -> CliResult<Response> {
    let Some(id) = response.state["result"]["id"].as_str().map(str::to_owned) else {
        return Ok(response);
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = require_ok(send(socket, &Command::State)?)?.state;
        let found = state["entries"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| entry["id"] == id));
        if found {
            let mut refreshed = response;
            refreshed.state = state;
            refreshed.state["result"] = json!({ "id": id });
            return Ok(refreshed);
        }
        if Instant::now() >= deadline {
            return Err(CliError::Message(format!(
                "The app did not register the note ({id}). Check the app log beside the session data."
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn cmd(socket: &Path, raw: &str) -> CliResult<()> {
    let command: Command = serde_json::from_str(raw)
        .map_err(|error| CliError::Message(format!("Invalid protocol command: {error}")))?;
    let response = require_ok(send(socket, &command)?)?;
    print_command_response(&command, &response)
}

fn batch(socket: &Path, file: &Path) -> CliResult<()> {
    let contents = std::fs::read_to_string(file)?;
    let commands: Vec<Command> = serde_json::from_str(&contents)
        .map_err(|error| CliError::Message(format!("Invalid batch file: {error}")))?;
    let mut responses = Vec::with_capacity(commands.len());
    for command in &commands {
        let response = send(socket, command)?;
        // Keep a batch deterministic when it depends on a just-added note.
        let response = if response.ok && matches!(command, Command::AddNote { .. }) {
            wait_for_added_note(socket, response)?
        } else {
            response
        };
        responses.push(response);
    }
    print_json(&responses)?;
    let failures = responses.iter().filter(|response| !response.ok).count();
    if failures > 0 {
        return Err(CliError::Message(format!(
            "{failures} of {} commands failed.",
            responses.len()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Session lifecycle
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct Session {
    socket: PathBuf,
    data_dir: PathBuf,
    pid: u32,
    started_at: u64,
}

fn start(cli_socket: Option<PathBuf>, args: StartArgs) -> CliResult<()> {
    let seeds = match args.seed.as_deref() {
        Some(raw) => parse_seed(raw)?,
        None => Vec::new(),
    };
    let data_dir = args.data_dir.clone().unwrap_or_else(default_data_dir);
    std::fs::create_dir_all(&data_dir)?;
    let socket = cli_socket
        .or_else(socket_from_env)
        .unwrap_or_else(|| data_dir.join("control.sock"));
    if Client::new(&socket)
        .with_timeout(Duration::from_secs(1))
        .ping()
    {
        return Err(CliError::Message(format!(
            "Another Worktable session is already listening on {}. Stop it or choose a different --socket.",
            socket.display()
        )));
    }
    let app_bin = resolve_app_bin(args.app_bin.as_deref())?;
    let database = data_dir.join("worktable.db");
    let media = data_dir.join("media");
    // Write app diagnostics beside the isolated data instead of inheriting the
    // caller's pipes: a launched GUI app must not keep an agent's captured
    // stdout/stderr open after `start` returns.
    let log_path = data_dir.join("worktable.log");
    let stderr = match std::fs::File::create(&log_path) {
        Ok(file) => Stdio::from(file),
        Err(_) => Stdio::null(),
    };

    let mut child = ProcessCommand::new(&app_bin)
        .env("WORKTABLE_DB_PATH", &database)
        .env("WORKTABLE_MEDIA_DIR", &media)
        .env("WORKTABLE_CONTROL_SOCKET", &socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .map_err(|error| {
            CliError::Message(format!(
                "Couldn't launch {}: {error}. Build it with `cargo build -p worktable-app` or set WORKTABLE_APP_BIN.",
                app_bin.display()
            ))
        })?;
    let pid = child.id();

    let result = start_session(&mut child, &socket, &data_dir, &seeds, args.keep_onboarding);
    let session = match result {
        Ok(session) => session,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&socket);
            return Err(error);
        }
    };

    let session = Session {
        socket: session.socket,
        data_dir: session.data_dir,
        pid,
        started_at: unix_millis(),
    };
    write_session(&session)?;
    print_json(&json!({
        "socket": session.socket,
        "data_dir": session.data_dir,
        "pid": session.pid,
    }))
}

fn start_session(
    child: &mut Child,
    socket: &Path,
    data_dir: &Path,
    seeds: &[(String, Option<String>)],
    keep_onboarding: bool,
) -> CliResult<Session> {
    let client = Client::new(socket).with_timeout(Duration::from_secs(2));
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if client.ping() {
            break;
        }
        if let Some(status) = child.try_wait()? {
            return Err(CliError::Message(format!(
                "The app exited during startup ({status}). See {}.",
                data_dir.join("worktable.log").display()
            )));
        }
        if Instant::now() >= deadline {
            return Err(CliError::Message(format!(
                "Timed out waiting for the control socket at {}. See {}.",
                socket.display(),
                data_dir.join("worktable.log").display()
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if !keep_onboarding {
        skip_onboarding(socket)?;
    }

    if !seeds.is_empty() {
        let mut ids = Vec::with_capacity(seeds.len());
        for (content, title) in seeds {
            let response = require_ok(send(
                socket,
                &Command::AddNote {
                    content: content.clone(),
                    title: title.clone(),
                },
            )?)?;
            if let Some(id) = response.state["result"]["id"].as_str() {
                ids.push(id.to_owned());
            }
        }
        wait_for_entries(socket, &ids)?;
    }

    Ok(Session {
        socket: socket.to_path_buf(),
        data_dir: data_dir.to_path_buf(),
        pid: child.id(),
        started_at: unix_millis(),
    })
}

/// Dismiss the first-run tour and verify it stays dismissed: config reads run
/// asynchronously at startup, so a skip can race a stale "not completed" read.
fn skip_onboarding(socket: &Path) -> CliResult<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        require_ok(send(
            socket,
            &Command::Onboarding {
                intent: OnboardingAction::Skip,
            },
        )?)?;
        std::thread::sleep(Duration::from_millis(200));
        let state = require_ok(send(socket, &Command::State)?)?.state;
        if state["onboarding"]["step"].is_null() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(CliError::Message(
                "The first-run tour would not dismiss. Pass --keep-onboarding to drive it instead."
                    .to_owned(),
            ));
        }
    }
}

fn wait_for_entries(socket: &Path, ids: &[String]) -> CliResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = require_ok(send(socket, &Command::State)?)?.state;
        let present: HashSet<&str> = state["entries"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry["id"].as_str())
                    .collect()
            })
            .unwrap_or_default();
        if ids.iter().all(|id| present.contains(id.as_str())) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(CliError::Message(
                "The app did not register the seeded notes in time.".to_owned(),
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn parse_seed(raw: &str) -> CliResult<Vec<(String, Option<String>)>> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SeedItem {
        Text(String),
        Entry {
            content: String,
            #[serde(default)]
            title: Option<String>,
        },
    }

    let items: Vec<SeedItem> = serde_json::from_str(raw)
        .map_err(|error| CliError::Message(format!("Invalid --seed JSON: {error}")))?;
    let mut seeds = Vec::with_capacity(items.len());
    for item in items {
        let (content, title) = match item {
            SeedItem::Text(content) => (content, None),
            SeedItem::Entry { content, title } => (content, title),
        };
        let content = content.trim().to_owned();
        if content.is_empty() {
            return Err(CliError::Message(
                "Every --seed item needs non-empty content.".to_owned(),
            ));
        }
        seeds.push((content, title));
    }
    Ok(seeds)
}

fn stop(socket: &Path) -> CliResult<()> {
    let session = read_session(socket);
    let client = Client::new(socket).with_timeout(Duration::from_secs(2));
    let mut was_running = false;
    if socket.exists() {
        // Already gone between the existence check and the connect is a
        // successful stop.
        if let Ok(response) = client.send(&Command::Quit) {
            require_ok(response)?;
            was_running = true;
        }
    }

    if was_running {
        let deadline = Instant::now() + STOP_TIMEOUT;
        while client.ping() {
            if Instant::now() >= deadline {
                return Err(CliError::Message(format!(
                    "Timed out waiting for the app to release {}.",
                    socket.display()
                )));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    let pid = session.as_ref().map(|session| session.pid);
    if let Some(pid) = pid {
        wait_for_pid(pid, Duration::from_secs(5));
    }
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_file(session_path(socket));
    print_json(&json!({ "stopped": was_running, "pid": pid }))
}

fn wait_for_pid(pid: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let alive = ProcessCommand::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !alive || Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn session_path(socket: &Path) -> PathBuf {
    PathBuf::from(format!("{}.session.json", socket.display()))
}

fn write_session(session: &Session) -> CliResult<()> {
    let path = session_path(&session.socket);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, pretty(session))?;
    Ok(())
}

fn read_session(socket: &Path) -> Option<Session> {
    let raw = std::fs::read_to_string(session_path(socket)).ok()?;
    serde_json::from_str(&raw).ok()
}

// ---------------------------------------------------------------------------
// Screenshot
// ---------------------------------------------------------------------------

fn screenshot(socket: &Path, args: ScreenshotArgs) -> CliResult<()> {
    require_ok(send(
        socket,
        &Command::Window {
            intent: WindowAction::Focus,
        },
    )?)?;
    let state = require_ok(send(socket, &Command::State)?)?.state;
    let bounds = &state["window"]["bounds"];
    let x = bounds["x"]
        .as_f64()
        .ok_or_else(|| CliError::Message("The app did not report window bounds.".to_owned()))?;
    let y = bounds["y"].as_f64().unwrap_or(0.0);
    let width = bounds["width"].as_f64().unwrap_or(0.0).max(1.0);
    let height = bounds["height"].as_f64().unwrap_or(0.0).max(1.0);
    let region = format!(
        "{},{},{},{}",
        x.round() as i64,
        y.round() as i64,
        width.round() as i64,
        height.round() as i64
    );

    let path = args.path.unwrap_or_else(|| {
        let data_dir = read_session(socket)
            .map(|session| session.data_dir)
            .unwrap_or_else(std::env::temp_dir);
        data_dir.join(format!("screenshot-{}.png", unix_millis()))
    });
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let output = ProcessCommand::new("screencapture")
        .arg("-x")
        .arg(format!("-R{region}"))
        .arg(&path)
        .output()
        .map_err(|error| CliError::Message(format!("Couldn't run screencapture: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let detail = if stderr.is_empty() {
            output.status.to_string()
        } else {
            stderr.to_owned()
        };
        return Err(CliError::Message(format!(
            "screencapture failed ({detail}). Grant Screen Recording permission to the terminal or agent and unlock/wake the display, then retry."
        )));
    }
    let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    if size == 0 {
        return Err(CliError::Message(format!(
            "screencapture wrote an empty file at {}. Check Screen Recording permission.",
            path.display()
        )));
    }

    print_json(&json!({
        "path": path,
        "bounds": bounds,
        "region": region,
    }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn resolve_app_bin(explicit: Option<&Path>) -> CliResult<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("WORKTABLE_APP_BIN")
        && !path.is_empty()
    {
        return Ok(PathBuf::from(path));
    }
    let current = std::env::current_exe().map_err(|error| {
        CliError::Message(format!("Couldn't resolve the CLI executable: {error}"))
    })?;
    let sibling = current.with_file_name("worktable-app");
    if sibling.is_file() {
        return Ok(sibling);
    }
    Err(CliError::Message(format!(
        "Couldn't find the Worktable app next to {}. Build it with `cargo build -p worktable-app` or set WORKTABLE_APP_BIN.",
        current.display()
    )))
}

fn default_data_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "worktable-cli-{}-{}",
        std::process::id(),
        unix_millis()
    ))
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Command {
        let cli = Cli::try_parse_from(arguments).unwrap();
        cli.command
            .protocol_command()
            .expect("test command is a protocol command")
    }

    #[test]
    fn subcommands_map_to_protocol_commands() {
        assert_eq!(
            parse(&["worktable-cli", "page", "assistant"]),
            Command::Page {
                page: Page::Assistant
            }
        );
        assert_eq!(
            parse(&["worktable-cli", "settings", "providers"]),
            Command::Settings {
                tab: Some(SettingsTab::Providers)
            }
        );
        assert_eq!(
            parse(&["worktable-cli", "settings"]),
            Command::Settings { tab: None }
        );
        assert_eq!(
            parse(&[
                "worktable-cli",
                "add-note",
                "Buy milk",
                "--title",
                "Groceries"
            ]),
            Command::AddNote {
                content: "Buy milk".into(),
                title: Some("Groceries".into()),
            }
        );
        assert_eq!(
            parse(&["worktable-cli", "label", "amber", "--id", "entry-1"]),
            Command::Label {
                id: Some("entry-1".into()),
                color: LabelColor::Amber,
            }
        );
        assert_eq!(
            parse(&["worktable-cli", "label", "none"]),
            Command::Label {
                id: None,
                color: LabelColor::None,
            }
        );
        assert_eq!(
            parse(&["worktable-cli", "sort", "alpha", "--descending"]),
            Command::Sort {
                mode: SortMode::Alpha,
                ascending: Some(false),
            }
        );
        assert_eq!(
            parse(&["worktable-cli", "sort", "time"]),
            Command::Sort {
                mode: SortMode::Time,
                ascending: None,
            }
        );
        assert_eq!(
            parse(&["worktable-cli", "assistant", "ask", "What did I save?"]),
            Command::Assistant {
                intent: AssistantAction::Ask,
                text: Some("What did I save?".into()),
            }
        );
        assert_eq!(
            parse(&["worktable-cli", "knowledge", "build"]),
            Command::Knowledge {
                intent: KnowledgeAction::Build
            }
        );
        assert_eq!(
            parse(&["worktable-cli", "chats", "new"]),
            Command::Chats {
                intent: ChatsAction::New
            }
        );
        assert_eq!(
            parse(&["worktable-cli", "window", "focus"]),
            Command::Window {
                intent: WindowAction::Focus
            }
        );
        assert_eq!(
            parse(&["worktable-cli", "background", "off"]),
            Command::Background { enabled: false }
        );
        assert_eq!(
            parse(&["worktable-cli", "onboarding", "skip"]),
            Command::Onboarding {
                intent: OnboardingAction::Skip
            }
        );
        assert_eq!(
            parse(&["worktable-cli", "theme", "dark"]),
            Command::Theme {
                mode: ThemeMode::Dark
            }
        );
        assert_eq!(
            parse(&["worktable-cli", "clear-search"]),
            Command::ClearSearch
        );
        assert_eq!(parse(&["worktable-cli", "quit"]), Command::Quit);
    }

    #[test]
    fn shell_local_commands_do_not_map_to_the_socket() {
        for arguments in [
            vec!["worktable-cli", "start"],
            vec!["worktable-cli", "stop"],
            vec!["worktable-cli", "screenshot"],
            vec!["worktable-cli", "batch", "/tmp/commands.json"],
            vec!["worktable-cli", "cmd", "{\"action\":\"ping\"}"],
        ] {
            let cli = Cli::try_parse_from(arguments).unwrap();
            assert!(
                cli.command.protocol_command().is_none(),
                "{:?} is shell-local",
                cli.command
            );
        }
    }

    #[test]
    fn seed_accepts_strings_and_objects() {
        let seeds = parse_seed(
            r#"["First note", {"content":"Second note","title":"A title"}, {"content":"Third"}]"#,
        )
        .unwrap();
        assert_eq!(
            seeds,
            vec![
                ("First note".to_owned(), None),
                ("Second note".to_owned(), Some("A title".to_owned())),
                ("Third".to_owned(), None),
            ]
        );
        assert!(parse_seed(r#"["  "]"#).is_err());
        assert!(parse_seed("not json").is_err());
    }
}
