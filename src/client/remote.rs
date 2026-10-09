//! Connecting: the local unix socket or a remote daemon (ssh bootstrap + sealed TCP), the first
//! Hello/Welcome exchange, and the reconnect policy.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::config;
use crate::proto::{self, FrameReader, FrameWriter, Rejected, VersionMismatch, C2S, PROTO_VERSION, S2C};

#[derive(Clone)]
pub struct Creds {
    pub port: u16,
    pub id: [u8; 16],
    pub key: [u8; 32],
}

#[derive(Clone)]
pub enum Target {
    Local,
    Remote { dest: String, ssh_args: Vec<String>, host: String },
}

impl Target {
    /// For a remote destination this runs `ssh -G` (before raw mode) to learn the real host name.
    pub fn remote(dest: &str, ssh_args: Vec<String>) -> Target {
        let host = resolve_host(dest, &ssh_args);
        Target::Remote { dest: dest.to_string(), ssh_args, host }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Target::Remote { .. })
    }
}

fn resolve_host(dest: &str, ssh_args: &[String]) -> String {
    let out = Command::new("ssh").args(ssh_args).arg("-G").arg(dest).stdin(Stdio::null()).stderr(Stdio::null()).output();
    if let Ok(o) = out
        && o.status.success()
        && let Some(h) = String::from_utf8_lossy(&o.stdout).lines().find_map(|l| l.strip_prefix("hostname "))
        && !h.trim().is_empty()
    {
        return h.trim().to_string();
    }
    dest.rsplit('@').next().unwrap_or(dest).to_string()
}

/// Remote command running `nt <sub>`. ssh runs it in a non-interactive shell, which usually skips the rc files
/// that put ~/.local/bin and ~/.cargo/bin on PATH, so add those (via `sh`, whatever the login shell is).
/// `NT_REMOTE_CMD` overrides the remote `nt` path.
fn remote_cmd(sub: &str) -> String {
    match std::env::var("NT_REMOTE_CMD") {
        Ok(cmd) => format!("{cmd} {sub}"),
        Err(_) => format!(r#"sh -c 'PATH="$PATH:$HOME/.local/bin:$HOME/.cargo/bin"; exec nt {sub}'"#),
    }
}

/// `nt restart <dest>`: run `nt server --restart` on the remote machine over ssh.
pub fn ssh_restart(dest: &str, ssh_args: &[String]) -> Result<()> {
    let status = Command::new("ssh")
        .args(ssh_args)
        .arg(dest)
        .arg(remote_cmd("server --restart"))
        .stdin(Stdio::null())
        .status()
        .context("running ssh")?;
    if status.code() == Some(127) {
        bail!("nt not found on {dest}: install it there (./install.sh --remote {dest}) or set NT_REMOTE_CMD=/path/to/nt");
    }
    if !status.success() {
        bail!("ssh restart failed ({status})");
    }
    println!("nt server restarted on {dest}");
    Ok(())
}

/// `ssh <args> <dest> nt bootstrap`. ssh's stderr and tty prompts pass through; the caller must be in cooked mode.
pub fn ssh_bootstrap(dest: &str, ssh_args: &[String]) -> Result<Creds> {
    let out = Command::new("ssh")
        .args(ssh_args)
        .arg(dest)
        .arg(remote_cmd("bootstrap"))
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .context("running ssh")?;
    if out.status.code() == Some(127) {
        bail!("nt not found on {dest}: install it there (./install.sh --remote {dest}) or set NT_REMOTE_CMD=/path/to/nt");
    }
    if !out.status.success() {
        bail!("ssh bootstrap failed ({})", out.status);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| l.starts_with("NT1 ")).context("unexpected bootstrap output")?;
    parse_creds(line).context("malformed bootstrap line")
}

pub fn parse_creds(line: &str) -> Option<Creds> {
    let mut it = line.split_whitespace();
    (it.next()? == "NT1").then_some(())?;
    Some(Creds { port: it.next()?.parse().ok()?, id: proto::unhex(it.next()?)?, key: proto::unhex(it.next()?)? })
}

pub fn bootstrap_line(port: u16, id: &[u8; 16], key: &[u8; 32]) -> String {
    format!("NT1 {port} {} {}", proto::hex(id), proto::hex(key))
}

pub struct Inbox {
    pub rx: mpsc::UnboundedReceiver<S2C>,
    task: JoinHandle<()>,
}

impl Drop for Inbox {
    fn drop(&mut self) {
        self.task.abort(); // also closes the read half of a half-open connection
    }
}

pub struct Link {
    pub wr: FrameWriter,
    pub inbox: Inbox,
    /// From `Welcome`: input with `seq <=` this has been applied by the server.
    pub welcome_seq: u64,
}

fn is_eof(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|e| {
        matches!(e.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe)
    })
}

/// Marker: the connection died after our Hello but before Welcome (e.g. a wrong key).
#[derive(Debug)]
struct HelloDropped;

impl std::fmt::Display for HelloDropped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "connection closed during the handshake")
    }
}

impl std::error::Error for HelloDropped {}

pub async fn connect_or_start_unix() -> Result<UnixStream> {
    let path = config::socket_path()?;
    if let Ok(s) = UnixStream::connect(&path).await {
        return Ok(s);
    }
    // `nt server` daemonizes itself, so this returns almost at once.
    let null = Stdio::null;
    std::process::Command::new(std::env::current_exe()?)
        .arg("server")
        .stdin(null())
        .stdout(null())
        .stderr(null())
        .status()?;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(s) = UnixStream::connect(&path).await {
            return Ok(s);
        }
    }
    bail!("could not start the server (see server.log in {})", path.parent().unwrap().display())
}

/// Open a connection and complete Hello/Welcome. `acked` = highest input seq known applied.
pub async fn open_link(target: &Target, creds: Option<&Creds>, acked: u64) -> Result<Link> {
    let (mut rd, mut wr) = match (target, creds) {
        (Target::Local, _) => {
            let (r, w) = connect_or_start_unix().await?.into_split();
            (FrameReader::new(Box::new(r), None), FrameWriter::new(Box::new(w), None))
        }
        (Target::Remote { host, .. }, Some(c)) => {
            let s = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect((host.as_str(), c.port)))
                .await
                .map_err(|_| anyhow!("connect to {host}:{} timed out", c.port))?
                .with_context(|| format!("connecting to {host}:{}", c.port))?;
            let _ = s.set_nodelay(true);
            let (r, w) = s.into_split();
            tokio::time::timeout(Duration::from_secs(10), proto::client_handshake(Box::new(r), Box::new(w), &c.id, &c.key))
                .await
                .map_err(|_| anyhow!("handshake timed out"))??
        }
        (Target::Remote { .. }, None) => bail!("no credentials"),
    };
    let sent = wr.write_msg(&C2S::Hello { proto: PROTO_VERSION, last_input_seq_acked: acked }).await;
    let welcome = match sent {
        Ok(()) => tokio::time::timeout(Duration::from_secs(10), rd.read_msg::<S2C>())
            .await
            .map_err(|_| anyhow!("no Welcome from the server"))?,
        Err(e) => Err(e),
    };
    let welcome_seq = match welcome {
        Ok(S2C::Welcome { last_input_seq, .. }) => last_input_seq,
        Ok(S2C::Error(e)) => return Err(VersionMismatch(e).into()),
        Ok(_) => bail!("server did not answer the handshake"),
        // Daemons from before the version message just hang up.
        Err(e) if !target.is_remote() && is_eof(&e) => {
            return Err(VersionMismatch(
                "the running nt daemon closed the connection during the handshake, most likely a different nt version; run `nt server --restart` to replace it (this ends its sessions)".into(),
            )
            .into())
        }
        Err(e) if target.is_remote() => return Err(e.context(HelloDropped)),
        Err(e) => return Err(e),
    };
    // Frames are read in their own task because `read_msg` is not cancel-safe.
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        while let Ok(m) = rd.read_msg::<S2C>().await {
            if tx.send(m).is_err() {
                break;
            }
        }
    });
    Ok(Link { wr, inbox: Inbox { rx, task }, welcome_seq })
}

/// Reconnect policy for a remote client: backoff 0.5 s doubling to 5 s, same credentials, ssh again only when
/// the server no longer knows us.
pub struct Connector {
    pub target: Target,
    pub creds: Option<Creds>,
    next_try: Instant,
    delay: Duration,
    hello_drops: u32,
    /// Set while the terminal is raw; ssh prompts then need cooked mode and stdin to ourselves.
    pub raw: bool,
    pub stdin_paused: Arc<AtomicBool>,
    pub acked: u64,
}

impl Connector {
    pub fn new(target: Target, creds: Option<Creds>, stdin_paused: Arc<AtomicBool>) -> Self {
        Connector {
            target,
            creds,
            next_try: Instant::now(),
            delay: Duration::from_millis(500),
            hello_drops: 0,
            raw: false,
            stdin_paused,
            acked: 0,
        }
    }

    /// One reconnect attempt, after the backoff delay. Cancel-safe: the delay is tracked as a deadline.
    pub async fn next(&mut self) -> Result<Link> {
        tokio::time::sleep_until(self.next_try).await;
        match self.attempt().await {
            Ok(l) => {
                self.delay = Duration::from_millis(500);
                self.hello_drops = 0;
                Ok(l)
            }
            Err(e) => {
                self.next_try = Instant::now() + self.delay;
                self.delay = (self.delay * 2).min(Duration::from_secs(5));
                Err(e)
            }
        }
    }

    async fn attempt(&mut self) -> Result<Link> {
        if self.creds.is_none() {
            self.rebootstrap()?;
        }
        match open_link(&self.target, self.creds.as_ref(), self.acked).await {
            Err(e) if e.is::<Rejected>() => {}
            // A wrong key looks like a dropped connection after Hello; repeated drops mean re-register.
            Err(e) if e.is::<HelloDropped>() => {
                self.hello_drops += 1;
                if self.hello_drops < 3 {
                    return Err(e);
                }
            }
            r => return r,
        }
        self.hello_drops = 0;
        self.rebootstrap()?;
        open_link(&self.target, self.creds.as_ref(), self.acked).await
    }

    fn rebootstrap(&mut self) -> Result<()> {
        let Target::Remote { dest, ssh_args, .. } = &self.target else { bail!("not a remote target") };
        self.creds = None;
        if self.raw {
            self.stdin_paused.store(true, Ordering::SeqCst);
            let _ = disable_raw_mode();
            eprint!("\r\nnt: registration lost, re-running ssh bootstrap\r\n");
        }
        let r = ssh_bootstrap(dest, ssh_args);
        if self.raw {
            let _ = enable_raw_mode();
            self.stdin_paused.store(false, Ordering::SeqCst);
        }
        self.creds = Some(r?);
        Ok(())
    }
}
