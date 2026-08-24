//! Supervision of the `agy` local server.
//!
//! Every other provider in this workspace is one request. This one is a process: the quota
//! lives behind an HTTPS server that the Antigravity CLI binds on loopback, so a poll has
//! to find a server, or start one, before it can ask anything. Experiment E2 measured what
//! that costs — 1.86 s to the first answer from cold, **7 ms** once warm — which is why the
//! effort here goes into lifecycle rather than into latency. There is nothing to tune about
//! a request that takes seven milliseconds.
//!
//! # The four things this module has to get right
//!
//! 1. **`agy` only stays bound when it thinks it has a terminal.** Spawned with its output
//!    on a pipe it exits; spawned on a pseudoterminal with no arguments it stays up and
//!    serves. So we allocate a pty and hand the child the slave side.
//! 2. **The port is not fixed and there are two of them.** The process binds two loopback
//!    sockets per run and only one answers the RPC. They are recovered from the kernel — the
//!    process's own socket inodes, matched against its network namespace's TCP tables — and
//!    then tried, rather than guessed.
//! 3. **A 200 is not proof of readiness.** Before authentication finishes the server
//!    answers `RetrieveUserQuotaSummary` with a structurally valid payload whose buckets all
//!    read `remainingFraction: 1` — indistinguishable by shape from a real answer and
//!    identical by value to a genuinely untouched quota. E2 cost two attempts to see this.
//!    The gate is [`USER_STATUS_PATH`], which says in words whether anyone is logged in.
//! 4. **A wedged server must be replaced, not retried.** One forced relaunch per poll, then
//!    the poll fails and the card keeps its last good reading. Retrying a bound-but-deaf
//!    socket forever is how a five-minute interval becomes a busy loop.
//!
//! # What is ours to kill
//!
//! A server we started is ours: it is tracked, kept warm between polls, and torn down with
//! its process group when this provider drops. A server that was already running is not —
//! the user's own editor may be holding it — so it is *adopted* for reads and never
//! signalled. Adoption is also what keeps a leak from compounding: a daemon killed with
//! `SIGKILL` cannot run a destructor, and the next start finds the orphan and uses it
//! rather than adding a second copy of a process that resident-sets 170 MB.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::providers::{ProviderError, http};

/// The RPC that reports quota. Posted with an empty JSON object as its body.
pub const QUOTA_SUMMARY_PATH: &str =
    "/exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary";

/// The RPC that says whether anyone is logged in. The readiness gate; see the module docs.
pub const USER_STATUS_PATH: &str = "/exa.language_server_pb.LanguageServerService/GetUserStatus";

/// The CLI that brings the server up.
const BINARY: &str = "agy";

/// How long a server we just started is given to authenticate.
///
/// E2 measured 1.85 s on this machine. The allowance is an order of magnitude above it
/// because the measurement is one machine's, and the cost of being wrong in this direction
/// is one slow poll while the cost of being wrong in the other is a card that never fills.
const COLD_READY_TIMEOUT: Duration = Duration::from_secs(20);

/// How long an already-running server is given to answer the gate before it is treated as
/// wedged. Warm is 7 ms, so anything near this is not a slow answer, it is no answer.
const WARM_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Gap between attempts at the gate.
const READY_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long a server gets to leave after `SIGTERM` before it is killed.
const TERM_GRACE: Duration = Duration::from_millis(300);

/// How long a process we started gets to bind a socket. E2 measured 0.31 s.
const BIND_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on the question "is this the socket that speaks the RPC". A loopback round trip
/// is milliseconds; the other socket refuses outright.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// A server that is up, reachable and logged in, with the answer that proved it.
#[derive(Debug)]
pub struct Ready {
    /// Loopback port serving the RPC.
    pub port: u16,
    /// The body [`USER_STATUS_PATH`] returned. Re-used rather than re-fetched: it carries
    /// the plan name the card shows, and asking twice for one poll's worth of truth would
    /// invite the two answers to disagree.
    pub status_body: String,
}

/// The `agy` server this provider talks to, across polls.
#[derive(Debug)]
pub struct Agy {
    client: reqwest::Client,
    server: Mutex<Option<Server>>,
}

impl Agy {
    /// Builds the supervisor. Nothing is started until [`Agy::ready`].
    pub fn new() -> Result<Self, ProviderError> {
        Ok(Self {
            client: loopback_client()?,
            server: Mutex::new(None),
        })
    }

    /// A server that is up and authenticated.
    ///
    /// Reuses the warm one, adopts a foreign one, or starts our own, in that order. A
    /// server that is bound but will not answer the gate is force-relaunched **once**; a
    /// second failure fails the poll rather than looping.
    pub async fn ready(&self) -> Result<Ready, ProviderError> {
        let mut held = self.server.lock().await;
        let mut last: Option<ProviderError> = None;
        for attempt in 0..2 {
            let relaunch = attempt > 0;
            let (port, cold) = self.connect(&mut held, relaunch).await?;
            let budget = if cold {
                COLD_READY_TIMEOUT
            } else {
                WARM_READY_TIMEOUT
            };
            match self.wait_ready(port, budget).await {
                Ok(status_body) => return Ok(Ready { port, status_body }),
                Err(Gate::LoggedOut(detail)) => {
                    // Not a fault of ours and not one a relaunch mends. Let go of the
                    // server rather than keeping 170 MB warm for a quota nobody can read.
                    tracing::info!(port, %detail, "the agy server has nobody logged in");
                    shutdown(held.take());
                    return Err(ProviderError::NoCredential);
                }
                Err(Gate::Unreachable(error)) => {
                    tracing::debug!(port, attempt, %error, "the agy server did not answer");
                    last = Some(error);
                }
            }
        }
        shutdown(held.take());
        Err(last.unwrap_or_else(|| {
            ProviderError::Local("the agy server did not answer after a relaunch".to_owned())
        }))
    }

    /// Posts one RPC to a server already proven ready.
    pub async fn rpc(&self, port: u16, path: &str) -> Result<String, ProviderError> {
        let response = self
            .client
            .post(endpoint(port, path))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .body("{}")
            .send()
            .await
            .map_err(ProviderError::Transport)?;
        let status = response.status();
        let retry_after = http::retry_after_header(&response).map(str::to_owned);
        http::check(status, retry_after.as_deref())?;
        response.text().await.map_err(ProviderError::Transport)
    }

    /// Returns the port of a live server, starting one if there is none.
    ///
    /// The flag says whether the server is newly started, which is the only thing that
    /// justifies waiting twenty seconds for it to authenticate.
    async fn connect(
        &self,
        held: &mut Option<Server>,
        relaunch: bool,
    ) -> Result<(u16, bool), ProviderError> {
        if relaunch {
            shutdown(held.take());
        }
        if let Some(server) = held.as_mut() {
            if server.is_alive() {
                return Ok((server.port, false));
            }
            *held = None;
        }
        // A foreign server first: one is enough per machine, and starting a second copy of
        // a process this size to ask it a question the first one can answer is rude.
        if !relaunch && let Some(server) = adopt(&self.client).await {
            let port = server.port;
            *held = Some(server);
            return Ok((port, false));
        }
        let server = spawn(&self.client).await?;
        let port = server.port;
        *held = Some(server);
        Ok((port, true))
    }

    /// Polls the gate until the server says someone is logged in, or the budget runs out.
    async fn wait_ready(&self, port: u16, budget: Duration) -> Result<String, Gate> {
        let deadline = Instant::now() + budget;
        loop {
            let last = match self.rpc(port, USER_STATUS_PATH).await {
                Ok(body) => match super::logged_in(&body) {
                    Ok(()) => return Ok(body),
                    Err(detail) => Gate::LoggedOut(detail),
                },
                Err(error) => Gate::Unreachable(error),
            };
            if Instant::now() >= deadline {
                return Err(last);
            }
            tokio::time::sleep(READY_POLL_INTERVAL).await;
        }
    }
}

impl Drop for Agy {
    fn drop(&mut self) {
        shutdown(self.server.get_mut().take());
    }
}

/// Why the gate did not open.
#[derive(Debug)]
enum Gate {
    /// The server answered, and the answer was that nobody is logged in.
    LoggedOut(String),
    /// The server did not answer, or did not answer this.
    Unreachable(ProviderError),
}

/// One `agy` process and how to reach it.
#[derive(Debug)]
struct Server {
    port: u16,
    pid: i32,
    /// The handle for a process we started. `None` for one we found running, which is
    /// somebody else's to end.
    own: Option<Owned>,
}

impl Server {
    fn is_alive(&mut self) -> bool {
        match self.own.as_mut() {
            Some(owned) => matches!(owned.child.try_wait(), Ok(None)),
            None => rustix::process::Pid::from_raw(self.pid)
                .map(|pid| rustix::process::test_kill_process(pid).is_ok())
                .unwrap_or(false),
        }
    }
}

/// A process we started: the child itself, and the pty keeping it convinced it has a
/// terminal.
#[derive(Debug)]
struct Owned {
    child: std::process::Child,
    /// Held so the master end stays open for the process's lifetime; the thread reading it
    /// ends when this closes.
    _pty: std::fs::File,
}

/// Ends a server we started. A server we adopted is only let go of.
fn shutdown(server: Option<Server>) {
    let Some(mut server) = server else { return };
    let Some(mut owned) = server.own.take() else {
        return;
    };
    // The whole group, because the child was given one of its own precisely so that
    // anything it started leaves with it.
    if let Some(pid) = rustix::process::Pid::from_raw(server.pid) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::TERM);
    }
    let deadline = Instant::now() + TERM_GRACE;
    while Instant::now() < deadline {
        if matches!(owned.child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = owned.child.kill();
    let _ = owned.child.wait();
}

/// Starts `agy` on a pseudoterminal and finds the port it answers on.
async fn spawn(client: &reqwest::Client) -> Result<Server, ProviderError> {
    let binary = resolve_binary().ok_or_else(|| {
        ProviderError::Local(format!(
            "the {BINARY} command is not on PATH, so there is no Antigravity server to ask"
        ))
    })?;
    let (master, slave) = open_pty()?;

    let mut command = Command::new(&binary);
    // No arguments, deliberately: `agy` with a subcommand does that job and exits, and it
    // is the bare interactive form that brings the language server up and leaves it bound.
    command
        .stdin(Stdio::from(dup(&slave)?))
        .stdout(Stdio::from(dup(&slave)?))
        .stderr(Stdio::from(dup(&slave)?))
        // Its own group, so `shutdown` can end the whole tree with one signal and so a
        // signal sent to *us* is not delivered to it by the terminal.
        .process_group(0)
        // Something that looks like a terminal to a program that checks.
        .env("TERM", "xterm-256color");
    // This process is the one place in this program that reaches a provider's servers
    // without being one of our own clients: `agy` does its own HTTP. The proxy therefore
    // has to arrive as environment, and it arrives *per command* — putting it on the
    // daemon's own unit would mean a restart to change it, and a restart takes every card
    // off the screen for as long as the first poll takes.
    if let Some(proxy) = http::proxy() {
        command.envs(proxy.child_env());
    }
    if let Some(home) = std::env::var_os("HOME") {
        command.current_dir(&home);
    }
    let child = command.spawn().map_err(|error| {
        ProviderError::Local(format!("could not start {}: {error}", binary.display()))
    })?;
    // Our copy of the slave has to go, or the master never reaches end-of-file when the
    // child exits and the draining thread outlives the process it was draining.
    drop(slave);

    let pid = i32::try_from(child.id()).unwrap_or(-1);
    drain(master.try_clone().map_err(|error| {
        ProviderError::Local(format!("could not watch the {BINARY} terminal: {error}"))
    })?);

    let mut owned = Owned {
        child,
        _pty: master,
    };
    match wait_for_port(client, pid).await {
        Some(port) => Ok(Server {
            port,
            pid,
            own: Some(owned),
        }),
        None => {
            let _ = owned.child.kill();
            let _ = owned.child.wait();
            Err(ProviderError::Local(format!(
                "{BINARY} started but never bound a port to talk to"
            )))
        }
    }
}

/// Finds an `agy` already running and picks the socket of its that speaks the RPC.
async fn adopt(client: &reqwest::Client) -> Option<Server> {
    for pid in running_agy_pids() {
        for port in listening_ports(pid) {
            if speaks_rpc(client, port).await {
                tracing::debug!(pid, port, "adopted a running agy server");
                return Some(Server {
                    port,
                    pid,
                    own: None,
                });
            }
        }
    }
    None
}

/// Waits for a freshly started process to bind, then picks the socket that answers.
///
/// E2 measured both ports listening 0.31 s after the spawn. The loop is short and frequent
/// for that reason: the wait is over almost immediately or it is not a wait at all.
async fn wait_for_port(client: &reqwest::Client, pid: i32) -> Option<u16> {
    let deadline = Instant::now() + BIND_TIMEOUT;
    loop {
        for port in listening_ports(pid) {
            if speaks_rpc(client, port).await {
                return Some(port);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// True when this port answers the language server's own RPC.
///
/// The two sockets a run binds are told apart by asking, not by position: one of them is
/// something else entirely and refuses the connection. Any HTTP answer counts, including an
/// unauthenticated one — this establishes *which socket*, and readiness is a separate
/// question with its own gate.
async fn speaks_rpc(client: &reqwest::Client, port: u16) -> bool {
    let answered = client
        .post(endpoint(port, USER_STATUS_PATH))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("{}")
        .timeout(PROBE_TIMEOUT)
        .send()
        .await;
    matches!(answered, Ok(response) if response.status().as_u16() < 500)
}

/// The URL for one RPC. Always `127.0.0.1`: the port came from a socket table that also
/// lists sockets we have no business talking to, and the loopback pin is what makes that
/// safe.
fn endpoint(port: u16, path: &str) -> String {
    format!("https://127.0.0.1:{port}{path}")
}

/// A client that will talk to the loopback server's self-signed certificate.
///
/// The exception is why every provider owns its client rather than sharing one. It is
/// bounded by the address: nothing built here is ever pointed at a name that resolves off
/// this machine, so the certificate that is not checked is one that could only have been
/// presented by a process on the same host.
///
/// `no_proxy` for the same reason, and it is not redundant: this builder is not
/// [`http::builder`], but `system-proxy` would still have it read `HTTPS_PROXY` out of the
/// environment and ask a proxy to connect to *our own* `127.0.0.1`.
fn loopback_client() -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .user_agent(tidemark_types::user_agent())
        .timeout(http::REQUEST_TIMEOUT)
        .connect_timeout(http::CONNECT_TIMEOUT)
        .tls_danger_accept_invalid_certs(true)
        .no_proxy()
        .build()
        .map_err(ProviderError::Client)
}

/// Allocates a pseudoterminal and returns both ends.
///
/// **`O_NOCTTY` on the slave is the whole reason this is a function with a comment.**
/// Opening a terminal device from a session leader that has no controlling terminal makes
/// that terminal the session's — and a systemd user unit is exactly such a leader. The
/// daemon would adopt this pty as its console, and the moment `agy` exited the kernel would
/// hang up the session: `SIGHUP`, delivered to `tidemarkd`, which dies. That is not a
/// theory. It is what happened on the first run of this provider from the installed
/// package, where the daemon lasted thirty-one seconds and left no error behind it, because
/// there was no error — it was hung up on. Running the same code from a shell hides it
/// completely: a terminal session already has a controlling terminal, so there is nothing
/// for the open to steal.
fn open_pty() -> Result<(std::fs::File, std::fs::File), ProviderError> {
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
    use std::os::unix::fs::OpenOptionsExt as _;
    let fail = |what: &str, error: rustix::io::Errno| {
        ProviderError::Local(format!("could not {what} a pseudoterminal: {error}"))
    };
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)
        .map_err(|error| fail("open", error))?;
    grantpt(&master).map_err(|error| fail("grant", error))?;
    unlockpt(&master).map_err(|error| fail("unlock", error))?;
    let name = ptsname(&master, Vec::new()).map_err(|error| fail("name", error))?;
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(OsStr::new(std::str::from_utf8(name.as_bytes()).map_err(
            |_| ProviderError::Local("a pseudoterminal has no name".to_owned()),
        )?))
        .map_err(|error| {
            ProviderError::Local(format!("could not open a pseudoterminal: {error}"))
        })?;
    Ok((std::fs::File::from(master), slave))
}

fn dup(file: &std::fs::File) -> Result<std::fs::File, ProviderError> {
    file.try_clone()
        .map_err(|error| ProviderError::Local(format!("could not duplicate a terminal: {error}")))
}

/// Reads everything the child writes to its terminal and throws it away.
///
/// Discarded rather than kept, and that is a decision rather than laziness: a terminal
/// session of a vendor CLI is not a stream this project can promise contains no
/// credentials, and the only thing an error message would gain by quoting it is the chance
/// to put one in a log. What the draining is *for* is that a pty buffer fills at about
/// 64 KiB, and a child blocked on a write nobody is reading is a server that has stopped
/// answering for a reason that has nothing to do with quota.
fn drain(mut master: std::fs::File) {
    std::thread::Builder::new()
        .name("agy-pty-drain".to_owned())
        .spawn(move || {
            let mut scratch = [0_u8; 4096];
            while matches!(master.read(&mut scratch), Ok(n) if n > 0) {}
        })
        .ok();
}

/// Where `agy` is, if it is anywhere.
fn resolve_binary() -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(BINARY))
            .find(|candidate| is_executable(candidate))
    });
    on_path.or_else(|| {
        // Its own installer's default, which is not always on a systemd user unit's PATH.
        let fallback = PathBuf::from(std::env::var_os("HOME")?)
            .join(".local")
            .join("bin")
            .join(BINARY);
        is_executable(&fallback).then_some(fallback)
    })
}

/// Whether the local fallback can be attempted without trying to start a process.
pub fn is_available() -> bool {
    resolve_binary().is_some()
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Every `agy` process this user is running.
fn running_agy_pids() -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut pids: Vec<i32> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| is_agy(*pid))
        .collect();
    // Youngest first: if two are running, the one most recently started is the one whose
    // session is current.
    pids.sort_unstable_by_key(|pid| std::cmp::Reverse(*pid));
    pids
}

/// True when this pid is an `agy`.
///
/// `exe` is the honest answer and is only readable for our own processes, which is exactly
/// the set we are willing to talk to. `comm` is the fallback and is truncated to fifteen
/// bytes by the kernel — harmless for a three-letter name.
fn is_agy(pid: i32) -> bool {
    if let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) {
        return exe.file_name() == Some(OsStr::new(BINARY));
    }
    false
}

/// The loopback ports one process is listening on.
///
/// The kernel has no per-process socket list, so this is the join the tooling does: the
/// inodes behind the process's own socket descriptors, matched against the TCP tables of
/// its network namespace. `ss` and `lsof` do the same thing; doing it here keeps a poll
/// from depending on a tool that minimal installs leave out.
fn listening_ports(pid: i32) -> Vec<u16> {
    let inodes = socket_inodes(pid);
    if inodes.is_empty() {
        return Vec::new();
    }
    let mut ports = Vec::new();
    for table in ["tcp", "tcp6"] {
        if let Ok(content) = std::fs::read_to_string(format!("/proc/{pid}/net/{table}")) {
            ports.extend(listening_in_table(&content, &inodes));
        }
    }
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// The socket inodes behind a process's open descriptors.
fn socket_inodes(pid: i32) -> HashSet<String> {
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return HashSet::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let target = std::fs::read_link(entry.path()).ok()?;
            socket_inode(target.to_str()?).map(str::to_owned)
        })
        .collect()
}

/// The inode out of a `socket:[12345]` symlink target, or `None` for any other descriptor.
fn socket_inode(target: &str) -> Option<&str> {
    target
        .strip_prefix("socket:[")?
        .strip_suffix(']')
        .filter(|inode| !inode.is_empty())
}

/// Ports of the `LISTEN` rows of one `/proc/<pid>/net/tcp*` table whose inode is ours.
///
/// Columns are `sl local_address rem_address st ... inode`; `st` is `0A` for a listening
/// socket and the local address is `HEXADDR:HEXPORT`.
fn listening_in_table(content: &str, inodes: &HashSet<String>) -> Vec<u16> {
    const LISTEN: &str = "0A";
    content
        .lines()
        .filter_map(|line| {
            let columns: Vec<&str> = line.split_whitespace().collect();
            if columns.len() < 10 || columns[3] != LISTEN || !inodes.contains(columns[9]) {
                return None;
            }
            let (_, port) = columns[1].rsplit_once(':')?;
            u16::from_str_radix(port, 16).ok().filter(|port| *port != 0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_descriptor_is_told_apart_from_every_other_kind() {
        assert_eq!(socket_inode("socket:[41269]"), Some("41269"));
        assert_eq!(socket_inode("/home/someone/.bashrc"), None);
        assert_eq!(socket_inode("anon_inode:[eventpoll]"), None);
        assert_eq!(socket_inode("socket:[]"), None);
    }

    /// Two rows of a real table: one listening socket that is ours, one established
    /// connection that is also ours. Only the first is a port anything can be asked on.
    const TABLE: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:B0BF 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41269 1 0 20 0
   1: 0100007F:B1BF 0100007F:C350 01 00000000:00000000 00:00000000 00000000  1000        0 41270 1 0 20 0
   2: 0100007F:B2BF 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 99999 1 0 20 0";

    #[test]
    fn only_listening_sockets_this_process_owns_become_ports() {
        let ours: HashSet<String> = ["41269", "41270"].iter().map(|s| s.to_string()).collect();
        // 0xB0BF is 45247. The established row is skipped for its state, the third row for
        // belonging to somebody else.
        assert_eq!(listening_in_table(TABLE, &ours), vec![45_247]);
    }

    #[test]
    fn a_table_we_own_nothing_in_yields_nothing() {
        assert!(listening_in_table(TABLE, &HashSet::new()).is_empty());
        assert!(listening_in_table("", &HashSet::new()).is_empty());
    }

    #[test]
    fn a_truncated_table_is_read_past_rather_than_panicked_on() {
        let ours: HashSet<String> = ["41269"].iter().map(|s| s.to_string()).collect();
        assert!(listening_in_table("  sl local_address\n   0: 0100007F:B0BF", &ours).is_empty());
    }

    #[test]
    fn requests_are_pinned_to_loopback() {
        // The port is recovered from a socket table, so the host must never be.
        assert_eq!(
            endpoint(45_503, USER_STATUS_PATH),
            "https://127.0.0.1:45503/exa.language_server_pb.LanguageServerService/GetUserStatus"
        );
        assert!(endpoint(1, QUOTA_SUMMARY_PATH).starts_with("https://127.0.0.1:1/"));
    }

    #[test]
    fn a_pseudoterminal_can_be_allocated_and_both_ends_are_usable() {
        // The one fact the spawn path rests on that a test can check without `agy`.
        let (master, slave) = open_pty().expect("a pty is available");
        assert!(dup(&slave).is_ok());
        drop((master, slave));
    }

    #[test]
    fn the_binary_is_looked_for_on_path_before_anywhere_else() {
        // Not asserted against the real `agy`, which may or may not be installed here.
        assert!(!is_executable(Path::new("/proc/self/environ")));
        assert!(is_executable(Path::new("/bin/sh")));
    }
}
