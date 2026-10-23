#![forbid(unsafe_code)]

//! `notif-ipc` — Unix-domain socket server for the notif control interface.
//!
//! # Protocol
//! One JSON object per line, request → response:
//!
//! | Request                          | Response                              |
//! |----------------------------------|---------------------------------------|
//! | `{"cmd":"dismiss-all"}`          | `{"ok":true}`                         |
//! | `{"cmd":"close","id":5}`         | `{"ok":true}`                         |
//! | `{"cmd":"history"}`              | `{"ok":true,"history":[…]}`           |
//! | `{"cmd":"clear-history"}`        | `{"ok":true}`                         |
//! | `{"cmd":"toggle-dnd"}`           | `{"ok":true,"dnd":<new>}`             |
//! | `{"cmd":"toggle-center"}`        | `{"ok":true,"visible":<new>}`         |
//! | `{"cmd":"status"}`               | `{"ok":true,"status":{…}}`            |
//! | unknown / malformed              | `{"ok":false,"error":"…"}`            |
//!
//! The connection stays open after a response; the client may send multiple
//! requests before closing.  Malformed requests do **not** close the
//! connection.
//!
//! # Socket path
//! `$XDG_RUNTIME_DIR/notif.sock`.  A stale socket at that path — one nobody
//! answers on — is removed before binding; a live one is left alone and the
//! server refuses to start.  The socket is removed again on clean exit.

use std::path::Path;
use std::time::Duration;

use futures_lite::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use notif_types::{HistoryEntry, IpcCmd, StatusInfo};

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors from the IPC subsystem.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    /// `$XDG_RUNTIME_DIR` is not set in the environment.
    #[error("$XDG_RUNTIME_DIR is not set; cannot determine socket path")]
    NoRuntimeDir,
    /// Failed to bind the Unix socket.
    #[error("failed to bind IPC socket: {0}")]
    Bind(#[source] std::io::Error),
    /// The core channel was closed (daemon shutting down).
    #[error("core IPC channel closed")]
    CoreClosed,
    /// Another process is already answering on the socket path.
    #[error("another process is already serving {0:?}")]
    InUse(std::path::PathBuf),
}

// ── Public wire protocol types ────────────────────────────────────────────────

/// Public wire-protocol types shared between the IPC server and `notifctl`.
///
/// Every type here implements both [`serde::Serialize`] and
/// [`serde::Deserialize`] so that both sender and receiver can use the same
/// definitions.  The `serde` attributes are the canonical source of truth for
/// the JSON wire format — do **not** change them without a corresponding
/// protocol-version bump.
pub mod protocol {
    use notif_types::{HistoryEntry, StatusInfo};

    /// Outgoing JSON request (client → server).
    ///
    /// Serialized as a tagged object with `"cmd"` as the tag field, using
    /// kebab-case variant names (e.g. `{"cmd":"dismiss-all"}`).
    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(tag = "cmd", rename_all = "kebab-case")]
    pub enum Request {
        /// Dismiss all active notifications.
        DismissAll,
        /// Close a specific notification by ID.
        Close {
            /// The notification ID to close.
            id: u32,
        },
        /// Retrieve the notification history, newest first.
        History,
        /// Clear the entire notification history.
        ClearHistory,
        /// Toggle Do-Not-Disturb mode.
        ToggleDnd,
        /// Toggle the notification center panel.
        ToggleCenter,
        /// Query the current daemon status.
        Status,
    }

    /// `{"ok":true}` — fire-and-forget success response.
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    pub struct OkResponse {
        /// Always `true` for this response type.
        pub ok: bool,
    }

    /// `{"ok":false,"error":"…"}` — error response.
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    pub struct ErrResponse {
        /// Always `false` for this response type.
        pub ok: bool,
        /// Human-readable error description.
        pub error: String,
    }

    /// `{"ok":true,"history":[…]}` — history query response.
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    pub struct HistoryResponse {
        /// `true` on success.
        pub ok: bool,
        /// Notification history entries, newest first.
        pub history: Vec<HistoryEntry>,
    }

    /// `{"ok":true,"dnd":<bool>}` — DND toggle response.
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    pub struct DndResponse {
        /// `true` on success.
        pub ok: bool,
        /// The **new** Do-Not-Disturb state after the toggle.
        pub dnd: bool,
    }

    /// `{"ok":true,"visible":<bool>}` — center toggle response.
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    pub struct VisibleResponse {
        /// `true` on success.
        pub ok: bool,
        /// The **new** visibility state of the notification center panel.
        pub visible: bool,
    }

    /// `{"ok":true,"status":{…}}` — status query response.
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    pub struct StatusResponse {
        /// `true` on success.
        pub ok: bool,
        /// Snapshot of the current daemon status.
        pub status: StatusInfo,
    }
}

// Re-export the protocol types under shorter aliases for internal use.
use protocol::{
    DndResponse, ErrResponse, HistoryResponse, OkResponse, Request, StatusResponse, VisibleResponse,
};

// ── Public API ────────────────────────────────────────────────────────────────

/// Run the IPC server at `$XDG_RUNTIME_DIR/notif.sock`.
///
/// Returns only on fatal setup errors ([`IpcError::NoRuntimeDir`],
/// [`IpcError::Bind`]) or when the core channel closes
/// ([`IpcError::CoreClosed`]).
pub async fn run(ipc_tx: async_channel::Sender<IpcCmd>) -> Result<(), IpcError> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or(IpcError::NoRuntimeDir)?;
    let path = std::path::PathBuf::from(runtime_dir).join("notif.sock");
    run_at(&path, ipc_tx).await
}

/// Run the IPC server at an explicit socket `path`.
///
/// Intended for tests that supply a temporary path without relying on
/// `$XDG_RUNTIME_DIR`.
///
/// A stale socket file at `path` is removed before binding; if something is
/// answering on it, this returns [`IpcError::InUse`] instead.  The socket is
/// removed again (best-effort) when this function returns **or is cancelled**:
/// notifd shuts the IPC task down by dropping its executor task, which drops
/// this future mid-await — cleanup therefore lives in a `Drop` guard, not in
/// straight-line code after the accept loop.
pub async fn run_at(path: &Path, ipc_tx: async_channel::Sender<IpcCmd>) -> Result<(), IpcError> {
    serve(path, ipc_tx, IDLE_TIMEOUT).await
}

/// How long a connection may sit without sending a whole request, or
/// without taking a response, before it is closed. `notifctl` sends its
/// one line the moment it connects; anything quiet for this long is not
/// going to say anything.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest request line accepted. The longest real one is a `close`
/// with a ten-digit id, well under a hundred bytes.
const MAX_REQUEST_BYTES: usize = 4096;

async fn serve(
    path: &Path,
    ipc_tx: async_channel::Sender<IpcCmd>,
    idle: Duration,
) -> Result<(), IpcError> {
    /// Unlinks the socket file when dropped (normal return AND task cancellation).
    struct SocketGuard(std::path::PathBuf);
    impl Drop for SocketGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    // A socket file is stale only if nobody answers on it. This used to
    // unlink it unconditionally, so a second notifd — one that went on to
    // lose the D-Bus name — took the first one's socket and then deleted
    // it on exit, leaving the running daemon reachable by nobody. Probe
    // first; refuse if something is listening. The connect is async and
    // bounded, because a listener with a full backlog would hold a
    // blocking one indefinitely — and an answer that slow still means
    // someone is there.
    let probe = within(
        Duration::from_secs(1),
        async_io::Async::<std::os::unix::net::UnixStream>::connect(path),
    )
    .await;
    match probe {
        Some(Ok(_)) | None => return Err(IpcError::InUse(path.to_path_buf())),
        // Refused (a stale file a crash left), missing, or not a socket at
        // all: nobody is being displaced. Remove it best-effort; a path we
        // cannot clear makes `bind` fail and say why.
        Some(Err(_)) => {
            let _ = std::fs::remove_file(path);
        }
    }

    // Bind and wrap in the async reactor.
    let listener = std::os::unix::net::UnixListener::bind(path).map_err(IpcError::Bind)?;
    let _guard = SocketGuard(path.to_path_buf());
    let listener = async_io::Async::new(listener).map_err(IpcError::Bind)?;

    log::info!("notif-ipc: listening at {path:?}");

    // Each connection runs as a task of its own. The loop used to await
    // one connection to the end before accepting the next, so a client
    // that connected and said nothing held every other client off until
    // it left. The executor is local to this future: dropping the future
    // (which is how notifd shuts the IPC task down) drops every
    // connection with it. A connection that finds the core gone says so
    // on `core_closed`, which ends the accept loop.
    let ex = async_executor::LocalExecutor::new();
    let (core_closed_tx, core_closed_rx) = async_channel::bounded::<()>(1);
    ex.run(async {
        loop {
            let accepted =
                futures_lite::future::or(async { Some(listener.accept().await) }, async {
                    let _ = core_closed_rx.recv().await;
                    None
                })
                .await;
            match accepted {
                None => {
                    log::info!("notif-ipc: core channel closed; exiting");
                    break;
                }
                Some(Ok((stream, _addr))) => {
                    let ipc_tx = ipc_tx.clone();
                    let core_closed_tx = core_closed_tx.clone();
                    ex.spawn(async move {
                        if !handle_connection(stream, &ipc_tx, idle).await {
                            let _ = core_closed_tx.try_send(());
                        }
                    })
                    .detach();
                }
                Some(Err(e)) => {
                    log::error!("notif-ipc: accept error: {e}");
                    if ipc_tx.is_closed() {
                        log::info!("notif-ipc: core channel closed after accept error; exiting");
                        break;
                    }
                }
            }
        }
    })
    .await;

    // Socket file removal happens in SocketGuard::drop.
    Err(IpcError::CoreClosed)
}

/// `fut`, or `None` if it has not finished within `limit`.
async fn within<T>(limit: Duration, fut: impl std::future::Future<Output = T>) -> Option<T> {
    futures_lite::future::or(async { Some(fut.await) }, async {
        async_io::Timer::after(limit).await;
        None
    })
    .await
}

// ── Per-connection handler ────────────────────────────────────────────────────

/// Handle one client connection.
///
/// Reads JSON request lines until the client closes the connection, and writes
/// one JSON response line per request.  Returns `false` if the core channel is
/// closed (the accept loop should stop); returns `true` to continue accepting.
///
/// A connection is closed when it sends nothing for `idle`, when a response
/// cannot be written within `idle`, and when a request line runs past
/// [`MAX_REQUEST_BYTES`] — `read_line` alone would buffer whatever arrived
/// before a newline, however much that was.
async fn handle_connection(
    stream: async_io::Async<std::os::unix::net::UnixStream>,
    ipc_tx: &async_channel::Sender<IpcCmd>,
    idle: Duration,
) -> bool {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();

    loop {
        line.clear();
        // One byte past the cap, so a line of exactly the cap is told apart
        // from one that was cut off.
        let mut limited = (&mut reader).take(MAX_REQUEST_BYTES as u64 + 1);
        let n = match within(idle, limited.read_line(&mut line)).await {
            None => {
                log::debug!("notif-ipc: connection idle past {idle:?}; closing");
                return true;
            }
            Some(Ok(0)) => {
                // Client closed connection.
                return true;
            }
            Some(Ok(n)) => n,
            Some(Err(e)) => {
                log::debug!("notif-ipc: read error on connection: {e}");
                return true;
            }
        };
        if n > MAX_REQUEST_BYTES {
            log::debug!("notif-ipc: request past {MAX_REQUEST_BYTES} bytes; closing");
            if let Ok(s) = serde_json::to_string(&ErrResponse {
                ok: false,
                error: format!("request longer than {MAX_REQUEST_BYTES} bytes"),
            }) {
                let _ = within(idle, write_line(&stream, &s)).await;
            }
            return true;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse request.
        let request: Result<Request, _> = serde_json::from_str(trimmed);
        let response_json = match request {
            Err(e) => {
                log::debug!("notif-ipc: malformed request {trimmed:?}: {e}");
                match serde_json::to_string(&ErrResponse {
                    ok: false,
                    error: format!("malformed request: {e}"),
                }) {
                    Ok(s) => s,
                    Err(e2) => {
                        log::error!("notif-ipc: failed to serialize error response: {e2}");
                        continue;
                    }
                }
            }
            Ok(req) => {
                match dispatch(req, ipc_tx).await {
                    DispatchResult::Response(json) => json,
                    DispatchResult::CoreClosed => {
                        // Write an error response so the client gets something,
                        // then signal the accept loop to stop.
                        if let Ok(s) = serde_json::to_string(&ErrResponse {
                            ok: false,
                            error: "daemon shutting down".into(),
                        }) {
                            let _ = within(idle, write_line(&stream, &s)).await;
                        }
                        return false;
                    }
                }
            }
        };

        match within(idle, write_line(&stream, &response_json)).await {
            Some(Ok(())) => {}
            // Client disappeared mid-write, or stopped reading.
            Some(Err(_)) | None => return true,
        }
    }
}

/// Write `payload` followed by a newline to `stream`.
async fn write_line(
    stream: &async_io::Async<std::os::unix::net::UnixStream>,
    payload: &str,
) -> std::io::Result<()> {
    let mut buf = String::with_capacity(payload.len() + 1);
    buf.push_str(payload);
    buf.push('\n');
    (&*stream).write_all(buf.as_bytes()).await
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

enum DispatchResult {
    Response(String),
    CoreClosed,
}

async fn dispatch(req: Request, ipc_tx: &async_channel::Sender<IpcCmd>) -> DispatchResult {
    match req {
        Request::DismissAll => {
            if ipc_tx.send(IpcCmd::DismissAll).await.is_err() {
                return DispatchResult::CoreClosed;
            }
            ok_response()
        }

        Request::Close { id } => {
            if ipc_tx.send(IpcCmd::Close { id }).await.is_err() {
                return DispatchResult::CoreClosed;
            }
            ok_response()
        }

        Request::History => {
            let (reply_tx, reply_rx) = async_channel::bounded::<Vec<HistoryEntry>>(1);
            if ipc_tx
                .send(IpcCmd::History { reply: reply_tx })
                .await
                .is_err()
            {
                return DispatchResult::CoreClosed;
            }
            match reply_rx.recv().await {
                Ok(entries) => serialize_response(&HistoryResponse {
                    ok: true,
                    history: entries,
                }),
                Err(_) => DispatchResult::CoreClosed,
            }
        }

        Request::ClearHistory => {
            if ipc_tx.send(IpcCmd::ClearHistory).await.is_err() {
                return DispatchResult::CoreClosed;
            }
            ok_response()
        }

        Request::ToggleDnd => {
            let (reply_tx, reply_rx) = async_channel::bounded::<bool>(1);
            if ipc_tx
                .send(IpcCmd::ToggleDnd { reply: reply_tx })
                .await
                .is_err()
            {
                return DispatchResult::CoreClosed;
            }
            match reply_rx.recv().await {
                Ok(dnd) => serialize_response(&DndResponse { ok: true, dnd }),
                Err(_) => DispatchResult::CoreClosed,
            }
        }

        Request::ToggleCenter => {
            let (reply_tx, reply_rx) = async_channel::bounded::<bool>(1);
            if ipc_tx
                .send(IpcCmd::ToggleCenter { reply: reply_tx })
                .await
                .is_err()
            {
                return DispatchResult::CoreClosed;
            }
            match reply_rx.recv().await {
                Ok(visible) => serialize_response(&VisibleResponse { ok: true, visible }),
                Err(_) => DispatchResult::CoreClosed,
            }
        }

        Request::Status => {
            let (reply_tx, reply_rx) = async_channel::bounded::<StatusInfo>(1);
            if ipc_tx
                .send(IpcCmd::Status { reply: reply_tx })
                .await
                .is_err()
            {
                return DispatchResult::CoreClosed;
            }
            match reply_rx.recv().await {
                Ok(status) => serialize_response(&StatusResponse { ok: true, status }),
                Err(_) => DispatchResult::CoreClosed,
            }
        }
    }
}

fn ok_response() -> DispatchResult {
    serialize_response(&OkResponse { ok: true })
}

fn serialize_response<T: serde::Serialize>(val: &T) -> DispatchResult {
    match serde_json::to_string(val) {
        Ok(s) => DispatchResult::Response(s),
        Err(e) => {
            log::error!("notif-ipc: failed to serialize response: {e}");
            // Fallback error JSON (hand-crafted to avoid another serde call).
            DispatchResult::Response(
                r#"{"ok":false,"error":"internal serialization error"}"#.into(),
            )
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use notif_types::{HistoryEntry, IpcCmd, StatusInfo, Urgency};
    use std::time::Duration;

    /// Fake core task: receives IpcCmd values and sends canned replies.
    async fn fake_core(ipc_rx: async_channel::Receiver<IpcCmd>) {
        while let Ok(cmd) = ipc_rx.recv().await {
            match cmd {
                IpcCmd::DismissAll => {}
                IpcCmd::Close { .. } => {}
                IpcCmd::ClearHistory => {}
                IpcCmd::History { reply } => {
                    let _ = reply
                        .send(vec![HistoryEntry {
                            id: 42,
                            app_name: "testapp".into(),
                            summary: "Test notification".into(),
                            body: "body text".into(),
                            urgency: Urgency::Normal,
                            created_at_unix: 1_000_000,
                        }])
                        .await;
                }
                IpcCmd::ToggleDnd { reply } => {
                    let _ = reply.send(true).await;
                }
                IpcCmd::ToggleCenter { reply } => {
                    let _ = reply.send(false).await;
                }
                IpcCmd::Status { reply } => {
                    let _ = reply
                        .send(StatusInfo {
                            dnd: true,
                            active: 2,
                            waiting: 1,
                            history: 5,
                            center_visible: false,
                        })
                        .await;
                }
            }
        }
    }

    /// Send one request line to the socket and return the trimmed response line.
    async fn do_cmd(path: &Path, request: &str) -> String {
        let stream = async_io::Async::<std::os::unix::net::UnixStream>::connect(path)
            .await
            .expect("connect to IPC socket");

        // Write request.
        (&stream)
            .write_all(format!("{request}\n").as_bytes())
            .await
            .expect("write request");

        // Read response line.
        let mut reader = BufReader::new(&stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read response");
        line.trim().to_owned()
    }

    #[test]
    fn test_ipc_protocol() {
        async_io::block_on(async {
            let ex = async_executor::LocalExecutor::new();
            let (ipc_tx, ipc_rx) = async_channel::unbounded::<IpcCmd>();
            let tmpdir = tempfile::TempDir::new().expect("tmpdir");
            let socket_path = tmpdir.path().join("notif-test.sock");

            let path_server = socket_path.clone();
            let tx_server = ipc_tx.clone();

            // Run everything inside the executor so spawned tasks are driven.
            ex.run(async {
                // Spawn the fake core handler.
                ex.spawn(fake_core(ipc_rx)).detach();

                // Spawn the IPC server.
                let path_for_server = path_server.clone();
                ex.spawn(async move {
                    let _ = run_at(&path_for_server, tx_server).await;
                })
                .detach();

                // Give the server a moment to bind.
                async_io::Timer::after(Duration::from_millis(50)).await;

                // ── dismiss-all ──────────────────────────────────────────────
                let resp = do_cmd(&path_server, r#"{"cmd":"dismiss-all"}"#).await;
                assert_eq!(resp, r#"{"ok":true}"#, "dismiss-all response");

                // ── close ────────────────────────────────────────────────────
                let resp = do_cmd(&path_server, r#"{"cmd":"close","id":5}"#).await;
                assert_eq!(resp, r#"{"ok":true}"#, "close response");

                // ── history ──────────────────────────────────────────────────
                let resp = do_cmd(&path_server, r#"{"cmd":"history"}"#).await;
                let v: serde_json::Value =
                    serde_json::from_str(&resp).expect("history response JSON");
                assert_eq!(v["ok"], true, "history ok");
                let entries = v["history"].as_array().expect("history array");
                assert_eq!(entries.len(), 1, "history entry count");
                assert_eq!(entries[0]["id"], 42, "history entry id");
                assert_eq!(
                    entries[0]["summary"], "Test notification",
                    "history entry summary"
                );
                assert_eq!(entries[0]["urgency"], "normal", "history entry urgency");

                // ── clear-history ────────────────────────────────────────────
                let resp = do_cmd(&path_server, r#"{"cmd":"clear-history"}"#).await;
                assert_eq!(resp, r#"{"ok":true}"#, "clear-history response");

                // ── toggle-dnd ───────────────────────────────────────────────
                let resp = do_cmd(&path_server, r#"{"cmd":"toggle-dnd"}"#).await;
                let v: serde_json::Value =
                    serde_json::from_str(&resp).expect("toggle-dnd response JSON");
                assert_eq!(v["ok"], true, "toggle-dnd ok");
                assert_eq!(v["dnd"], true, "toggle-dnd new state");

                // ── toggle-center ────────────────────────────────────────────
                let resp = do_cmd(&path_server, r#"{"cmd":"toggle-center"}"#).await;
                let v: serde_json::Value =
                    serde_json::from_str(&resp).expect("toggle-center response JSON");
                assert_eq!(v["ok"], true, "toggle-center ok");
                assert_eq!(v["visible"], false, "toggle-center new state");

                // ── status ───────────────────────────────────────────────────
                let resp = do_cmd(&path_server, r#"{"cmd":"status"}"#).await;
                let v: serde_json::Value =
                    serde_json::from_str(&resp).expect("status response JSON");
                assert_eq!(v["ok"], true, "status ok");
                assert_eq!(v["status"]["dnd"], true, "status dnd");
                assert_eq!(v["status"]["active"], 2, "status active");
                assert_eq!(v["status"]["waiting"], 1, "status waiting");
                assert_eq!(v["status"]["history"], 5, "status history");
                assert_eq!(
                    v["status"]["center_visible"], false,
                    "status center_visible"
                );

                // ── malformed JSON ────────────────────────────────────────────
                let resp = do_cmd(&path_server, r#"not-valid-json"#).await;
                let v: serde_json::Value =
                    serde_json::from_str(&resp).expect("error response JSON");
                assert_eq!(v["ok"], false, "malformed: ok=false");
                assert!(
                    v["error"].as_str().is_some(),
                    "malformed: error field present"
                );

                // ── unknown command ───────────────────────────────────────────
                let resp = do_cmd(&path_server, r#"{"cmd":"frobnicate"}"#).await;
                let v: serde_json::Value =
                    serde_json::from_str(&resp).expect("unknown cmd response JSON");
                assert_eq!(v["ok"], false, "unknown cmd: ok=false");
                assert!(
                    v["error"].as_str().is_some(),
                    "unknown cmd: error field present"
                );

                // ── multi-request on same connection ──────────────────────────
                {
                    let stream =
                        async_io::Async::<std::os::unix::net::UnixStream>::connect(&path_server)
                            .await
                            .expect("connect for multi-request test");

                    (&stream)
                        .write_all(b"{\"cmd\":\"dismiss-all\"}\n{\"cmd\":\"toggle-dnd\"}\n")
                        .await
                        .expect("write two requests");

                    let mut reader = BufReader::new(&stream);

                    let mut line1 = String::new();
                    reader.read_line(&mut line1).await.expect("read line 1");
                    assert_eq!(line1.trim(), r#"{"ok":true}"#, "multi-req line 1");

                    let mut line2 = String::new();
                    reader.read_line(&mut line2).await.expect("read line 2");
                    let v2: serde_json::Value =
                        serde_json::from_str(line2.trim()).expect("multi-req line 2 JSON");
                    assert_eq!(v2["ok"], true, "multi-req line 2 ok");
                    assert_eq!(v2["dnd"], true, "multi-req line 2 dnd");
                }
            })
            .await;
        });
    }

    /// Runs `body` against a server at a fresh socket path, with the fake
    /// core answering, and fails rather than hangs if it takes past `secs`.
    fn with_server<F, Fut>(idle: Duration, secs: u64, body: F)
    where
        F: FnOnce(std::path::PathBuf) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let tmpdir = tempfile::TempDir::new().expect("tmpdir");
        let path = tmpdir.path().join("notif-test.sock");
        with_server_inner(&path, idle, secs, body);
    }

    fn with_server_at<F, Fut>(path: &Path, body: F)
    where
        F: FnOnce(std::path::PathBuf) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_server_inner(path, Duration::from_secs(60), 5, body);
    }

    fn with_server_inner<F, Fut>(path: &Path, idle: Duration, secs: u64, body: F)
    where
        F: FnOnce(std::path::PathBuf) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let path = path.to_path_buf();
        async_io::block_on(async {
            let ex = async_executor::LocalExecutor::new();
            let (ipc_tx, ipc_rx) = async_channel::unbounded::<IpcCmd>();
            ex.run(async {
                ex.spawn(fake_core(ipc_rx)).detach();
                let server_path = path.clone();
                ex.spawn(async move {
                    let _ = serve(&server_path, ipc_tx, idle).await;
                })
                .detach();
                async_io::Timer::after(Duration::from_millis(50)).await;
                let finished = futures_lite::future::or(
                    async {
                        body(path.clone()).await;
                        true
                    },
                    async {
                        async_io::Timer::after(Duration::from_secs(secs)).await;
                        false
                    },
                )
                .await;
                assert!(finished, "the test did not finish within {secs}s");
            })
            .await;
        });
    }

    /// The accept loop awaited each connection to the end before taking
    /// the next, so one client that connected and said nothing wedged
    /// every other — notifctl, the tray, a keybind — until it left.
    #[test]
    fn a_silent_client_does_not_hold_up_the_next_one() {
        with_server(Duration::from_secs(60), 5, |path| async move {
            let _silent = async_io::Async::<std::os::unix::net::UnixStream>::connect(&path)
                .await
                .expect("connect the silent client");
            let resp = do_cmd(&path, r#"{"cmd":"dismiss-all"}"#).await;
            assert_eq!(resp, r#"{"ok":true}"#);
        });
    }

    #[test]
    fn an_idle_connection_is_closed_after_the_timeout() {
        with_server(Duration::from_millis(200), 5, |path| async move {
            let stream = async_io::Async::<std::os::unix::net::UnixStream>::connect(&path)
                .await
                .expect("connect");
            let mut buf = Vec::new();
            // EOF from the server is the close.
            let n = futures_lite::io::AsyncReadExt::read_to_end(&mut &stream, &mut buf)
                .await
                .expect("read until the server closes");
            assert_eq!(n, 0);
        });
    }

    /// `read_line` with no cap buffers whatever arrives before a newline.
    #[test]
    fn a_request_line_past_the_cap_is_refused_and_the_connection_closed() {
        with_server(Duration::from_secs(60), 5, |path| async move {
            let stream = async_io::Async::<std::os::unix::net::UnixStream>::connect(&path)
                .await
                .expect("connect");
            let long = vec![b'x'; MAX_REQUEST_BYTES + 10];
            // The server may close before taking all of it; that is fine.
            let _ = (&stream).write_all(&long).await;
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.expect("read response");
            let v: serde_json::Value = serde_json::from_str(line.trim()).expect("JSON");
            assert_eq!(v["ok"], false);
            line.clear();
            let n = reader.read_line(&mut line).await.unwrap_or(0);
            assert_eq!(n, 0, "the connection should be closed");
        });
    }

    /// A second notifd used to unlink the first one's socket and bind its
    /// own — and then delete it on exit — leaving the running daemon
    /// reachable by nobody. A live socket is now left alone.
    #[test]
    fn a_second_server_refuses_a_socket_someone_is_answering_on() {
        with_server(Duration::from_secs(60), 5, |path| async move {
            let (tx, _rx) = async_channel::unbounded::<IpcCmd>();
            let second = serve(&path, tx, Duration::from_secs(60)).await;
            assert!(matches!(second, Err(IpcError::InUse(_))));
            // And the first is still the one answering.
            let resp = do_cmd(&path, r#"{"cmd":"dismiss-all"}"#).await;
            assert_eq!(resp, r#"{"ok":true}"#);
        });
    }

    /// A socket file nobody is listening on is what a crash leaves
    /// behind; that one is still cleared and taken over.
    #[test]
    fn a_stale_socket_with_no_listener_is_replaced() {
        let tmpdir = tempfile::TempDir::new().expect("tmpdir");
        let path = tmpdir.path().join("stale.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).expect("bind"));
        assert!(path.exists(), "the stale socket file should be left");
        with_server_at(&path, |path| async move {
            let resp = do_cmd(&path, r#"{"cmd":"dismiss-all"}"#).await;
            assert_eq!(resp, r#"{"ok":true}"#);
        });
    }
}
