//! Gated diagnostic logging for the app shell.
//!
//! Logging is off by default. When the user enables *Diagnostic logs* in
//! Settings → General, the app appends `[unix_ms] [level] message` lines to a
//! file beside the database (`~/.worktable/logs/worktable.log` in production).
//! When logging is disabled nothing is written — there is deliberately no
//! `stderr` fallback, so a release build stays quiet.
//!
//! The logger never fails startup: an unreadable config leaves it disabled,
//! and an unwritable log directory is ignored.

use std::{
    fmt::Display,
    fs::{File, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

/// Config key for the diagnostics preference.
pub(crate) const LOGS_ENABLED_KEY: &str = "logs_enabled";

/// Environment override for the directory holding `worktable.log`.
const LOG_DIR_ENV: &str = "WORKTABLE_LOG_DIR";

const LOG_FILE_NAME: &str = "worktable.log";

#[derive(Clone, Copy)]
enum Level {
    Error,
    Warn,
    Info,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
        }
    }
}

#[derive(Default)]
struct LoggerState {
    enabled: bool,
    file: Option<File>,
}

/// An append-only diagnostic logger.
///
/// Instances are explicit so tests can point one at a temp directory without
/// touching the process-wide logger. The app uses [`init`] once at startup;
/// the module-level helpers route to that instance.
pub(crate) struct Logger {
    path: PathBuf,
    state: Mutex<LoggerState>,
}

impl Logger {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            state: Mutex::new(LoggerState::default()),
        }
    }

    /// The file this logger appends to.
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn enabled(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.enabled)
            .unwrap_or(false)
    }

    /// Turn logging on or off. Enabling creates the directory and file
    /// immediately; disabling closes the file and drops later writes.
    pub(crate) fn set_enabled(&self, enabled: bool) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.enabled = enabled;
        if enabled {
            if state.file.is_none() {
                state.file = open_log_file(&self.path);
            }
        } else {
            state.file = None;
        }
    }

    fn log(&self, level: Level, message: impl Display) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if !state.enabled {
            return;
        }
        if state.file.is_none() {
            state.file = open_log_file(&self.path);
        }
        let Some(file) = state.file.as_mut() else {
            return;
        };
        let line = format!("[{}] [{}] {}\n", unix_ms(), level.as_str(), message);
        let _ = file.write_all(line.as_bytes());
        let _ = file.flush();
    }

    pub(crate) fn error(&self, message: impl Display) {
        self.log(Level::Error, message);
    }

    pub(crate) fn warn(&self, message: impl Display) {
        self.log(Level::Warn, message);
    }

    pub(crate) fn info(&self, message: impl Display) {
        self.log(Level::Info, message);
    }
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// Enabling before [`init`] (a test view, or a preference read racing startup)
/// is preserved when the process logger is created.
static PENDING_ENABLED: AtomicBool = AtomicBool::new(false);

/// Install the process-wide logger and write the session-start line when the
/// persisted preference is on. Safe to call once; later calls reuse the
/// installed logger.
pub(crate) fn init(log_path: PathBuf, enabled: bool, database_path: &str) -> &'static Logger {
    let logger = LOGGER.get_or_init(|| Logger::new(log_path));
    let enabled = enabled || PENDING_ENABLED.load(Ordering::Relaxed);
    logger.set_enabled(enabled);
    if enabled {
        logger.info(format!(
            "Worktable {} started. Database: {database_path}",
            env!("CARGO_PKG_VERSION")
        ));
    }
    logger
}

/// Turn diagnostics on or off for the running app.
pub(crate) fn set_enabled(enabled: bool) {
    PENDING_ENABLED.store(enabled, Ordering::Relaxed);
    if let Some(logger) = LOGGER.get() {
        logger.set_enabled(enabled);
    }
}

pub(crate) fn enabled() -> bool {
    LOGGER
        .get()
        .map(Logger::enabled)
        .unwrap_or_else(|| PENDING_ENABLED.load(Ordering::Relaxed))
}

pub(crate) fn error(message: impl Display) {
    if let Some(logger) = LOGGER.get() {
        logger.error(message);
    }
}

pub(crate) fn warn(message: impl Display) {
    if let Some(logger) = LOGGER.get() {
        logger.warn(message);
    }
}

pub(crate) fn info(message: impl Display) {
    if let Some(logger) = LOGGER.get() {
        logger.info(message);
    }
}

/// Resolve the log file beside the database: `<database dir>/logs/worktable.log`.
///
/// `WORKTABLE_LOG_DIR` overrides the directory, which keeps CLI sessions and
/// tests out of the user's real `~/.worktable`.
pub(crate) fn log_path_for_database(database_path: &str) -> PathBuf {
    if let Ok(dir) = std::env::var(LOG_DIR_ENV)
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join(LOG_FILE_NAME);
    }
    let dir = Path::new(database_path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| parent.join("logs"))
        .unwrap_or_else(|| PathBuf::from("logs"));
    dir.join(LOG_FILE_NAME)
}

fn open_log_file(path: &Path) -> Option<File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).ok()?;
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_log_path(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "worktable-logs-{}-{}-{name}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        (dir.clone(), dir.join("logs").join(LOG_FILE_NAME))
    }

    #[test]
    fn disabled_logger_writes_no_file_and_no_bytes() {
        let (dir, path) = temp_log_path("disabled");
        let logger = Logger::new(&path);

        logger.info("hidden info");
        logger.warn("hidden warn");
        logger.error("hidden error");
        logger.set_enabled(false);
        logger.error("still hidden");

        assert!(!logger.enabled());
        assert!(
            !dir.exists(),
            "a disabled logger must not create its directory"
        );
        assert!(!path.exists(), "a disabled logger must not create its file");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn enabled_logger_appends_one_formatted_line_and_disabling_stops_it() {
        let (dir, path) = temp_log_path("enabled");
        let logger = Logger::new(&path);

        logger.set_enabled(true);
        assert!(logger.enabled());
        logger.info("diagnostics on");

        let contents = std::fs::read_to_string(logger.path()).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1, "one line per call, got: {contents:?}");
        assert!(lines[0].starts_with('['), "timestamp prefix: {lines:?}");
        assert!(lines[0].contains("[info]"), "level: {lines:?}");
        assert!(lines[0].contains("diagnostics on"), "message: {lines:?}");

        logger.set_enabled(false);
        logger.error("after disable");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 1, "disabled writes nothing new");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn log_path_sits_beside_the_database() {
        assert_eq!(
            log_path_for_database("/tmp/worktable-test/worktable.db"),
            PathBuf::from("/tmp/worktable-test/logs/worktable.log")
        );
        assert_eq!(
            log_path_for_database("worktable.db"),
            PathBuf::from("logs/worktable.log")
        );
    }
}
