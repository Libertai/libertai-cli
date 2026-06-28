//! Cross-process approval IPC for background/teammate agents.
//!
//! Background teammates run as DETACHED `libertai code --print` subprocesses
//! (own pid, no TTY — see `code_ui::start_background_agent`). Before this
//! module they used `PrintModeApprovalUi`, which auto-allows only
//! `team_task`/`mailbox` and auto-DENIES every other mutating tool — so a
//! teammate that needed `bash`/`edit`/`write` was silently denied and the
//! user was never asked ("approvals don't get passed from agents to main
//! process").
//!
//! This module routes a teammate's approval request back to the PARENT TUI:
//!
//! - **Parent (TUI)**: binds a `UnixListener` at a unique socket path
//!   ([`ApprovalServer`]), passed to children via the `LIBERTAI_APPROVAL_SOCKET`
//!   env var. `run_loop` polls it non-blocking each tick via
//!   [`ApprovalServer::poll_accept`] (alongside `agent_rx.try_recv()`). Each
//!   accepted request becomes an `ApprovalModal` the user decides; the choice
//!   is written back over the same connection ([`ApprovalResponder`]).
//! - **Child (teammate)**: [`IpcApprovalUi`] connects to the socket, sends the
//!   request, blocks reading the choice. If the parent is gone (connection
//!   refused / EOF / the env var is unset) it falls back to
//!   `PrintModeApprovalUi` (auto-deny) — preserving the safe headless
//!   behavior for teammates launched outside a TUI or after the parent exited.
//!
//! Wire protocol: length-prefixed JSON. Child→parent
//! `{id, tool, preview, always_rule}`; parent→child `{id, choice}`. A 4-byte
//! big-endian length prefixes each frame. `id` correlates the response to the
//! request (the child blocks on a single in-flight request per connection, so
//! `id` is belt-and-suspenders for future multiplexing).
//!
//! Lifecycle: parent death → child's `recv` returns 0 bytes → `Deny`. Parent
//! clean exit → listener dropped → unaccepted/ refused → `Deny`. No orphaned
//! approval ever hangs the child; no orphaned modal ever lingers in the parent.

#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::{mpsc, Arc, Mutex};

#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;
#[cfg(unix)]
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use crate::commands::code_approvals::ApprovalUi;
#[cfg(unix)]
use crate::commands::code_approvals::NotifyOutcome;
use crate::commands::code_approvals::PromptChoice;

/// Env var carrying the parent TUI's approval-socket path to a child.
pub const APPROVAL_SOCKET_ENV: &str = "LIBERTAI_APPROVAL_SOCKET";

// ---------- Wire protocol ----------

/// Child→parent request frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcApprovalRequest {
    pub id: String,
    pub tool: String,
    pub preview: String,
    pub always_rule: String,
}

/// Parent→child response frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcApprovalResponse {
    pub id: String,
    /// "allow" | "allow_session" | "always_allow" | "deny" |
    /// "prefix" | "grant_root" | "domain" (M4/#10 scope variants)
    pub choice: String,
}

impl IpcApprovalResponse {
    pub fn choice(&self) -> PromptChoice {
        match self.choice.as_str() {
            "allow" => PromptChoice::Allow,
            "allow_session" => PromptChoice::AllowSession,
            "always_allow" => PromptChoice::AlwaysAllow,
            // (M4/#10) Per-call scope variants forwarded over the IPC
            // channel from a teammate's parent TUI.
            "prefix" => PromptChoice::Prefix,
            "grant_root" => PromptChoice::GrantRoot,
            "domain" => PromptChoice::Domain,
            _ => PromptChoice::Deny,
        }
    }
}

/// Write a length-prefixed JSON frame.
#[cfg(unix)]
fn write_frame<W: Write>(w: &mut W, value: &impl Serialize) -> Result<()> {
    let json = serde_json::to_string(value)?;
    let bytes = json.as_bytes();
    let len = u32::try_from(bytes.len()).context("frame too large")?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(bytes)?;
    w.flush()?;
    Ok(())
}

/// Read a length-prefixed JSON frame. Returns `Ok(None)` on clean EOF
/// (peer closed) — the caller treats that as parent-gone → Deny.
#[cfg(unix)]
fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    // Cap to a sane maximum so a corrupted length field can't drive a
    // multi-GB allocation. 1 MiB is far above any realistic approval preview.
    const MAX_FRAME: usize = 1 << 20;
    if len > MAX_FRAME {
        anyhow::bail!("frame length {len} exceeds {MAX_FRAME}");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).context("reading frame body")?;
    let value = serde_json::from_slice(&buf).context("parsing frame")?;
    Ok(Some(value))
}

// ---------- Parent side ----------

/// A accepted teammate approval request + the socket to answer it on. The
/// parent's modal key handler resolves the user's `PromptChoice` and calls
/// [`ApprovalResponder::respond`].
pub struct ApprovalResponder {
    /// The connection to write the response to. Wrapped so the modal can
    /// hold it across the await on the user's keypress without borrowing.
    #[cfg(unix)]
    conn: Arc<Mutex<Option<UnixStream>>>,
    pub id: String,
}

impl ApprovalResponder {
    #[cfg(unix)]
    fn new(conn: UnixStream, id: String) -> Self {
        Self {
            conn: Arc::new(Mutex::new(Some(conn))),
            id,
        }
    }

    /// (Round-9) Test-only constructor: build a `Remote` responder from an
    /// arbitrary `UnixStream` so `app.rs` unit tests can drive
    /// `handle_approval_key`'s Remote (teammate) arm + assert the pre-modal
    /// phase is restored. `respond` is best-effort, so a test stream that is
    /// never read (or already closed) is harmless.
    #[cfg(all(unix, test))]
    pub(crate) fn for_test(conn: UnixStream, id: String) -> Self {
        Self::new(conn, id)
    }

    /// Send the user's choice to the teammate. Best-effort: a write failure
    /// (the teammate already exited) is silently ignored — the teammate's
    /// blocking read already returned EOF→Deny in that case.
    #[cfg(unix)]
    pub fn respond(&self, choice: PromptChoice) {
        let choice_str = match choice {
            PromptChoice::Allow => "allow",
            PromptChoice::AllowSession => "allow_session",
            PromptChoice::AlwaysAllow => "always_allow",
            // (M4/#10) Per-call scope variants.
            PromptChoice::Prefix => "prefix",
            PromptChoice::GrantRoot => "grant_root",
            PromptChoice::Domain => "domain",
            PromptChoice::Deny => "deny",
            PromptChoice::Paused { .. } => "deny",
        };
        if let Some(mut conn) = self.conn.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let resp = IpcApprovalResponse {
                id: self.id.clone(),
                choice: choice_str.to_string(),
            };
            let _ = write_frame(&mut conn, &resp);
            // Dropping `conn` closes the socket; the teammate's read returns.
            let _ = conn;
        }
    }
}

// ---------- Non-Unix (Windows) stubs ----------
//
// The approval IPC rides a Unix domain socket, which doesn't exist on
// Windows. Rather than gate the whole module off (the `Remote` responder
// variant is woven through `app.rs`'s modal handling), we keep the public
// types present on all platforms and make the socket I/O a no-op on
// non-Unix. Effect: on Windows the parent TUI never binds a socket
// (`bind` → `Err`, so `app.rs`'s `.ok()` gives `None` → the documented
// "fall back to auto-deny" pre-IPC path), and children's `from_env` returns
// `None`. No teammate approval is ever routed on Windows; behavior is the
// safe headless auto-deny. This mirrors how bind failure is already handled
// on Unix.
#[cfg(not(unix))]
impl ApprovalResponder {
    /// No-op on non-Unix: there's no socket to write to.
    pub fn respond(&self, _choice: PromptChoice) {}
}

/// Parent-side approval socket. Bound once at TUI startup; polled non-blocking
/// each `run_loop` tick. Drop closes the listener + removes the socket file.
pub struct ApprovalServer {
    #[cfg(unix)]
    listener: UnixListener,
    #[cfg(unix)]
    path: PathBuf,
}

#[cfg(unix)]
impl ApprovalServer {
    /// Bind a fresh Unix socket at a unique per-process path under the system
    /// temp dir. The path is returned so it can be passed to children via
    /// `APPROVAL_SOCKET_ENV`. A stale socket from a prior crashed TUI is
    /// unlinked before bind (EADDRINUSE otherwise). The temp dir (not the
    /// config dir) is used so the path stays under the 104-byte `sun_path`
    /// limit on macOS, where `~/Library/Application Support/libertai/...` can
    /// overflow it. Best-effort: if bind isn't usable the TUI still runs —
    /// teammates just fall back to auto-deny (the pre-IPC behavior).
    pub fn bind() -> Result<Self> {
        // Bind under the system temp dir, NOT the config dir. macOS' config
        // dir (`~/Library/Application Support/libertai`) is long enough that a
        // real $HOME (or a redirected one under `/var/folders/.../T/.tmpXXXX`)
        // pushes the socket path past the 104-byte `sun_path` limit, and
        // `UnixListener::bind` fails with "path must be shorter than SUN_LEN".
        // `env::temp_dir()` resolves to a short `/tmp`-rooted path on every
        // platform, so the unique per-process name stays well under the limit.
        // Mirrors code_image_tool's `libertai-image-tool-<pid>` temp-path
        // precedent.
        let dir = std::env::temp_dir();
        let _ = std::fs::create_dir_all(&dir);
        // Unique per-process path so two concurrent TUIs don't collide on a
        // shared socket (mirrors the round-7 cross-restart-collision lesson).
        let path = dir.join(format!("libertai-approval-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path); // stale socket from a prior run
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("binding approval socket at {}", path.display()))?;
        // Non-blocking so `run_loop`'s `poll_accept` is a cheap try, not a
        // blocking call that would stall the 80ms tick.
        listener.set_nonblocking(true)?;
        Ok(Self { listener, path })
    }

    /// The socket path to pass to children via `APPROVAL_SOCKET_ENV`.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Non-blocking accept. Returns a fully-read request + the responder if a
    /// child connected AND sent a complete frame; `Ok(None)` if no connection
    /// is pending this tick. A connection that closes before sending a frame
    /// is dropped silently (the child already gave up). Blocking on a single
    /// request read is acceptable: the child sends the frame immediately on
    /// connect, so the read returns promptly; if it doesn't (a misbehaving
    /// child), the read is non-blocking at the socket level so it returns
    /// `WouldBlock` and we drop the connection rather than stall the tick.
    pub fn poll_accept(&self) -> Result<Option<(IpcApprovalRequest, ApprovalResponder)>> {
        let (mut conn, _addr) = match self.listener.accept() {
            Ok(pair) => pair,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // The request frame is small + sent immediately; read it with a brief
        // bounded read. Keep the socket non-blocking and try once; if the
        // frame isn't fully here yet, drop the connection (the child will
        // retry by re-invoking the tool — but in practice the frame lands in
        // one TCP-sized send). Set a read timeout as a backstop.
        let _ = conn.set_read_timeout(Some(std::time::Duration::from_millis(500)));
        let req: Option<IpcApprovalRequest> = read_frame(&mut conn)?;
        let Some(req) = req else { return Ok(None) };
        // Restore non-blocking for the (idle) conn until we respond.
        let _ = conn.set_nonblocking(true);
        let responder = ApprovalResponder::new(conn, req.id.clone());
        Ok(Some((req, responder)))
    }
}

impl Drop for ApprovalServer {
    fn drop(&mut self) {
        // Closing the listener lets in-flight child connects refuse → Deny
        // (safe). Remove the socket file so it doesn't litter the temp dir.
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------- Non-Unix (Windows) `ApprovalServer` stub ----------
#[cfg(not(unix))]
impl ApprovalServer {
    /// Unix domain sockets don't exist on Windows, so bind always fails. The
    /// caller (`app.rs`) does `bind().ok()` and treats `None` as "fall back
    /// to auto-deny" — the safe pre-IPC headless behavior.
    pub fn bind() -> Result<Self> {
        anyhow::bail!("approval IPC is unavailable on non-Unix platforms");
    }

    /// Never reached on non-Unix (bind fails), but must compile.
    pub fn path(&self) -> &PathBuf {
        use std::sync::OnceLock;
        // Unreachable in practice (path() is only called after a successful
        // bind), but we need a valid `&PathBuf` to satisfy the signature.
        static EMPTY: OnceLock<PathBuf> = OnceLock::new();
        EMPTY.get_or_init(|| PathBuf::new())
    }

    /// No socket to poll; never yields a request.
    pub fn poll_accept(&self) -> Result<Option<(IpcApprovalRequest, ApprovalResponder)>> {
        Ok(None)
    }
}

// ---------- Child side ----------

/// `ApprovalUi` that routes a teammate's approval to the parent TUI over the
/// approval socket. Constructed only when `APPROVAL_SOCKET_ENV` is set; on
/// any failure to connect/send/receive it returns `Deny` (safe — mirrors the
/// `RatatuiApprovalUi` `unwrap_or(Deny)` discipline). Smart approvals are off
/// (the parent TUI decides, not a headless LLM).
pub struct IpcApprovalUi {
    #[cfg(unix)]
    socket_path: PathBuf,
}

impl IpcApprovalUi {
    /// Construct from the env var. `Ok(None)` if the var is unset (the caller
    /// falls back to `PrintModeApprovalUi`).
    #[cfg(unix)]
    pub fn from_env() -> Option<Self> {
        let path = std::env::var(APPROVAL_SOCKET_ENV).ok()?;
        let path = path.trim();
        if path.is_empty() {
            return None;
        }
        Some(Self {
            socket_path: PathBuf::from(path),
        })
    }

    /// Non-Unix: the parent TUI never binds a socket (see `ApprovalServer::bind`),
    /// so there is no IPC to route to. Always returns `None` so the caller
    /// falls back to the safe headless `PrintModeApprovalUi` (auto-deny).
    #[cfg(not(unix))]
    pub fn from_env() -> Option<Self> {
        None
    }

    #[cfg(unix)]
    fn decide_over_socket(
        &self,
        tool_name: &str,
        preview: &str,
        always_rule: &str,
    ) -> PromptChoice {
        let req = IpcApprovalRequest {
            // Unique id per request so a future multiplexed protocol can
            // correlate. Today one request per connection, so a fixed-ish id
            // is fine; include a counter + pid for uniqueness.
            id: format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ),
            tool: tool_name.to_string(),
            preview: preview.to_string(),
            always_rule: always_rule.to_string(),
        };
        let Ok(mut conn) = UnixStream::connect(&self.socket_path) else {
            return PromptChoice::Deny; // parent gone or not listening
        };
        if write_frame(&mut conn, &req).is_err() {
            return PromptChoice::Deny;
        }
        // Block until the parent responds. A `read_frame` of `None` is a clean
        // EOF (parent exited) → Deny. A parse error → Deny.
        match read_frame::<_, IpcApprovalResponse>(&mut conn) {
            Ok(Some(resp)) => resp.choice(),
            Ok(None) => PromptChoice::Deny,
            Err(_) => PromptChoice::Deny,
        }
    }
}

#[cfg(unix)]
#[async_trait]
impl ApprovalUi for IpcApprovalUi {
    fn allows_smart_approval(&self) -> bool {
        false
    }

    async fn decide(&self, tool_name: &str, preview: &str, always_rule: &str) -> PromptChoice {
        // The socket I/O is blocking; run it on a spawnable thread + await
        // the oneshot so this fits the async trait without blocking the
        // child's single-threaded runtime (mirrors LlmSmartApproval's shape
        // in code_aux.rs).
        let socket_path = self.socket_path.clone();
        let tool_name = tool_name.to_string();
        let preview = preview.to_string();
        let always_rule = always_rule.to_string();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let ui = IpcApprovalUi { socket_path };
            let choice = ui.decide_over_socket(&tool_name, &preview, &always_rule);
            let _ = tx.send(choice);
        });
        rx.recv().unwrap_or(PromptChoice::Deny)
    }

    async fn notify(&self, _title: &str, _body: &str) -> NotifyOutcome {
        NotifyOutcome::Skipped("IPC_NOTIFY_NOT_SUPPORTED".to_string())
    }
}

/// Non-Unix `ApprovalUi` for `IpcApprovalUi`. `from_env()` returns `None` on
/// non-Unix (see above), so this impl is never reached at runtime — it exists
/// only so `code.rs`'s `Arc::new(ipc)` used as `Arc<dyn ApprovalUi>` typechecks
/// on Windows. If it were ever called it would auto-deny (the safe headless
/// behavior), mirroring the Unix failure-path.
#[cfg(not(unix))]
#[async_trait::async_trait]
impl crate::commands::code_approvals::ApprovalUi for IpcApprovalUi {
    fn allows_smart_approval(&self) -> bool {
        false
    }

    async fn decide(&self, _tool_name: &str, _preview: &str, _always_rule: &str) -> PromptChoice {
        PromptChoice::Deny
    }

    async fn notify(
        &self,
        _title: &str,
        _body: &str,
    ) -> crate::commands::code_approvals::NotifyOutcome {
        crate::commands::code_approvals::NotifyOutcome::Skipped(
            "IPC_NOTIFY_NOT_SUPPORTED".to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::net::UnixStream;
    #[cfg(unix)]
    use std::sync::mpsc;
    #[cfg(unix)]
    use std::time::Duration;

    /// Spin up an `ApprovalServer`, connect a raw client, send a request
    /// frame, drive `poll_accept` to read it, respond, and confirm the client
    /// reads the choice. Pins the full round-trip + the wire protocol.
    #[cfg(unix)]
    #[test]
    fn roundtrip_request_response_over_socket() {
        let server = ApprovalServer::bind().expect("bind");
        let path = server.path().clone();

        let (tx, rx) = mpsc::channel();
        let client_thread = std::thread::spawn(move || {
            let mut conn = UnixStream::connect(&path).expect("connect");
            let req = IpcApprovalRequest {
                id: "req-1".into(),
                tool: "bash".into(),
                preview: "bash(rm -rf /)".into(),
                always_rule: "bash(rm *)".into(),
            };
            write_frame(&mut conn, &req).expect("write req");
            let resp: IpcApprovalResponse =
                read_frame(&mut conn).expect("read resp").expect("non-None");
            tx.send(resp).expect("send");
        });

        // Poll until the server accepts the request (the child connects
        // asynchronously). Bounded retries so the test can't hang.
        let mut accepted = None;
        for _ in 0..200 {
            if let Ok(Some((req, responder))) = server.poll_accept() {
                assert_eq!(req.tool, "bash");
                assert_eq!(req.preview, "bash(rm -rf /)");
                assert_eq!(req.id, "req-1");
                responder.respond(PromptChoice::AlwaysAllow);
                accepted = Some(responder);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(accepted.is_some(), "server accepted the request");

        client_thread.join().expect("client thread");
        let resp = rx.recv_timeout(Duration::from_secs(2)).expect("resp");
        assert_eq!(resp.id, "req-1");
        assert_eq!(resp.choice(), PromptChoice::AlwaysAllow);
    }

    /// A child whose parent never accepts returns `Deny` (safe fallback).
    #[cfg(unix)]
    #[test]
    fn child_denies_when_parent_socket_absent() {
        let ui = IpcApprovalUi {
            socket_path: PathBuf::from("/tmp/libertai-approval-does-not-exist.sock"),
        };
        let choice = ui.decide_over_socket("bash", "bash(echo hi)", "bash(echo *)");
        assert_eq!(choice, PromptChoice::Deny);
    }

    /// `from_env` returns None when the env var is unset (caller falls back).
    #[test]
    fn from_env_none_when_unset() {
        // Remove the var if a parent shell happened to set it.
        std::env::remove_var(APPROVAL_SOCKET_ENV);
        assert!(IpcApprovalUi::from_env().is_none());
    }

    /// `IpcApprovalResponse::choice` parses all four choices + defaults Deny.
    #[test]
    fn response_choice_parses() {
        assert_eq!(
            IpcApprovalResponse {
                id: "x".into(),
                choice: "allow".into()
            }
            .choice(),
            PromptChoice::Allow
        );
        assert_eq!(
            IpcApprovalResponse {
                id: "x".into(),
                choice: "allow_session".into()
            }
            .choice(),
            PromptChoice::AllowSession
        );
        assert_eq!(
            IpcApprovalResponse {
                id: "x".into(),
                choice: "always_allow".into()
            }
            .choice(),
            PromptChoice::AlwaysAllow
        );
        assert_eq!(
            IpcApprovalResponse {
                id: "x".into(),
                choice: "deny".into()
            }
            .choice(),
            PromptChoice::Deny
        );
        // Unknown choice → Deny (defensive).
        assert_eq!(
            IpcApprovalResponse {
                id: "x".into(),
                choice: "bogus".into()
            }
            .choice(),
            PromptChoice::Deny
        );
    }
}
