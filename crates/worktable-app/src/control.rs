//! Local control socket for agent-friendly automation.
//!
//! The server exists only when `WORKTABLE_CONTROL_SOCKET` is set. It binds a
//! Unix socket with owner-only permissions, accepts one JSON request per
//! connection on a background thread, and hands each request to the GPUI main
//! thread through [`ControlRequest`]. The main loop in `main.rs` executes it
//! against the real window and sends the [`Response`] back over the reply
//! channel, so the app's view and database stay the single source of truth.

use std::{
    io::{BufRead as _, BufReader, Write as _},
    os::unix::{
        fs::PermissionsExt as _,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::mpsc as std_mpsc,
    time::Duration,
};

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use worktable_control::{Command, Response};

/// How long the socket thread waits for the UI thread to execute a command.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// One command waiting for execution on the GPUI main thread.
pub(crate) struct ControlRequest {
    pub(crate) command: Command,
    pub(crate) reply: std_mpsc::Sender<Response>,
}

/// Start the control server when the environment asks for it.
///
/// A failure is logged and the app keeps running without the socket: the
/// optional automation surface must never block the window.
pub(crate) fn start_from_env() -> Option<(UnboundedReceiver<ControlRequest>, PathBuf)> {
    let path = worktable_control::socket_from_env()?;
    match start(&path) {
        Ok(server) => Some(server),
        Err(error) => {
            crate::logging::error(format!(
                "failed to start the control socket at {}: {error:#}",
                path.display()
            ));
            None
        }
    }
}

/// Bind the control socket and start its accept thread.
pub(crate) fn start(path: &Path) -> anyhow::Result<(UnboundedReceiver<ControlRequest>, PathBuf)> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    // A leftover path from a crashed app is stale; a path that still answers is
    // another live app and must not be unlinked behind its back.
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            anyhow::bail!(
                "another Worktable process is already listening on {}",
                path.display()
            );
        }
        std::fs::remove_file(path)?;
    }

    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;

    let (sender, receiver) = mpsc::unbounded_channel();
    let socket_path = path.to_path_buf();
    std::thread::Builder::new()
        .name("worktable-control".to_owned())
        .spawn(move || serve(listener, sender))
        .map_err(|error| anyhow::anyhow!("failed to spawn the control thread: {error}"))?;

    Ok((receiver, socket_path))
}

/// Remove the socket path after a clean quit (best effort).
pub(crate) fn remove_socket(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        crate::logging::warn(format!(
            "failed to remove the control socket at {}: {error}",
            path.display()
        ));
    }
}

/// Accept connections forever, one request per connection.
fn serve(listener: UnixListener, sender: UnboundedSender<ControlRequest>) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                crate::logging::error(format!("control socket accept failed: {error}"));
                continue;
            }
        };
        let sender = sender.clone();
        std::thread::Builder::new()
            .name("worktable-control-request".to_owned())
            .spawn(move || handle_connection(stream, sender))
            .ok();
    }
}

fn handle_connection(stream: UnixStream, sender: UnboundedSender<ControlRequest>) {
    let _ = stream.set_read_timeout(Some(REPLY_TIMEOUT));
    let _ = stream.set_write_timeout(Some(REPLY_TIMEOUT));

    let mut line = String::new();
    let command = match BufReader::new(match stream.try_clone() {
        Ok(reader) => reader,
        Err(error) => {
            crate::logging::error(format!("control socket clone failed: {error}"));
            return;
        }
    })
    .read_line(&mut line)
    {
        Ok(0) => return,
        Ok(_) => match serde_json::from_str::<Command>(line.trim()) {
            Ok(command) => command,
            Err(error) => {
                write_response(
                    stream,
                    &Response::error(format!("Invalid control command: {error}")),
                );
                return;
            }
        },
        Err(error) => {
            write_response(
                stream,
                &Response::error(format!("Could not read the control command: {error}")),
            );
            return;
        }
    };

    let (reply_sender, reply_receiver) = std_mpsc::channel();
    if sender
        .send(ControlRequest {
            command,
            reply: reply_sender,
        })
        .is_err()
    {
        write_response(
            stream,
            &Response::error("The Worktable window is not available."),
        );
        return;
    }

    let response = reply_receiver
        .recv_timeout(REPLY_TIMEOUT)
        .unwrap_or_else(|_| Response::error("The app did not answer the control request in time."));
    write_response(stream, &response);
}

fn write_response(mut stream: UnixStream, response: &Response) {
    let payload = match serde_json::to_vec(response) {
        Ok(payload) => payload,
        Err(error) => format!(
            "{{\"ok\":false,\"error\":\"could not encode the control reply: {error}\",\"state\":null}}"
        )
        .into_bytes(),
    };
    let _ = stream.write_all(&payload);
    let _ = stream.write_all(b"\n");
    let _ = stream.flush();
}
