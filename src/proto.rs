//! Wire protocol: `u32 BE length` + postcard payload. Same messages locally and over TCP.

use anyhow::{anyhow, bail, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTO_VERSION: u32 = 6;
pub const MAX_FRAME: usize = 1 << 20;

pub type SessionId = u32;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum NameSource {
    Initial,
    Heuristic,
    Llm,
    /// Locked: never auto-renamed.
    User,
}

/// The folder-tree group a session belongs to (see `server/groups.rs`): a project root, or the shallowest of several
/// nested working directories.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct GroupInfo {
    /// Absolute path of the group's root directory.
    pub key: String,
    /// Basename of `key` (the whole path when it is outside the server's $HOME).
    pub label: String,
    /// `key` with the server's $HOME shown as `~` (the client does not know the server's home).
    pub shown: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionInfo {
    pub id: SessionId,
    pub name: String,
    pub name_source: NameSource,
    pub created_unix: u64,
    pub last_active_unix: u64,
    /// Last time a client attached or typed into the session (the switcher orders by this).
    pub last_used_unix: u64,
    pub clients: u32,
    pub cols: u16,
    pub rows: u16,
    pub cwd: String,
    pub fg_cmd: String,
    /// LLM topic: the session's goal in at most 50 characters; empty until the first naming run.
    pub recap: String,
    /// LLM rolling summary of what has been done in the session (at most 300 characters); empty until the first run.
    pub summary: String,
    /// The shell has exited; the session is kept until closed or respawned.
    pub exited: bool,
    /// Live status line (`✓ cargo build · {ago}`). `{dur}` / `{ago}` are filled in by the client
    /// from `status_since` (see `expand_status`), so the line stays current without extra pushes.
    pub status: String,
    /// Unix time `{dur}` / `{ago}` count from.
    pub status_since: u64,
    /// CPU use of all processes in the session, percent of one core (may exceed 100).
    pub cpu_pct: f32,
    /// Resident memory of all processes in the session (shared pages counted per process).
    pub rss_bytes: u64,
    /// A naming run that will update the topic and summary is in progress for this session.
    pub pending: bool,
    /// The folder-tree group of the session's current directory; `None` = ungrouped.
    pub group: Option<GroupInfo>,
}

/// Compact elapsed time: `45s`, `3m12s`, `1h05m`.
pub fn fmt_dur(s: u64) -> String {
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

/// `5s ago`, `2m ago`, `3h ago`, `2d ago`.
pub fn fmt_ago(s: u64) -> String {
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

/// Fill the time placeholders of a status line.
pub fn expand_status(status: &str, since: u64, now: u64) -> String {
    let d = now.saturating_sub(since);
    status.replace("{dur}", &fmt_dur(d)).replace("{ago}", &fmt_ago(d))
}

/// One full-text search hit: the matching transcript line (trimmed around the match).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SearchHit {
    pub session: SessionId,
    pub line: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum C2S {
    /// First message on every connection.
    Hello { proto: u32, last_input_seq_acked: u64 },
    List,
    /// `session: None` creates a new session, started in `cwd` (the daemon falls back to `$HOME` when it is absent
    /// or not an existing directory).
    Attach {
        session: Option<SessionId>,
        cols: u16,
        rows: u16,
        term: String,
        resume_from: Option<u64>,
        cwd: Option<String>,
    },
    Detach,
    /// `seq` increases monotonically per client identity.
    Input { seq: u64, data: Vec<u8> },
    Resize { cols: u16, rows: u16 },
    /// Empty name = back to automatic naming.
    Rename { session: SessionId, name: String },
    Kill { session: SessionId },
    Ping,
    /// Control socket only (refused over TCP): create a remote-client registration.
    Register,
    /// Case-insensitive substring search over the session transcripts.
    Search { query: String },
    /// Start a new shell in an exited session (same id, same transcript).
    Respawn { session: SessionId },
    /// The switcher is open (true) or closed (false): resource usage is sampled faster while watched.
    Watch(bool),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum S2C {
    /// The client drops queued input with `seq <= last_input_seq`.
    Welcome { proto: u32, last_input_seq: u64 },
    /// Reply to `List`; also pushed when names or the session set change.
    Sessions(Vec<SessionInfo>),
    Attached { session: SessionId },
    /// `offset` = absolute position of `data[0]` in the session stream.
    Output { offset: u64, data: Vec<u8> },
    /// Full repaint; the stream continues at `offset`.
    Redraw { offset: u64, data: Vec<u8> },
    SessionEnded { session: SessionId },
    Error(String),
    Pong,
    /// Periodic input ack (sent before every `Pong`): the client may drop queued input with `seq <= last_input_seq`.
    Ack { last_input_seq: u64 },
    /// Reply to `Register`.
    Registered { port: u16, client_id: [u8; 16], key: [u8; 32] },
    /// Reply to `Search`.
    SearchResults(Vec<SearchHit>),
    /// The last naming error (empty = none); sent before `Sessions` in the reply to `List`, on `Watch(true)` and when it changes.
    NamerError(String),
    /// Why naming is paused right now (`battery`, `low memory`, `memory pressure`; empty = not paused); sent like `NamerError`.
    NamerPaused(String),
}


pub const TAG_LEN: usize = 16;

/// One direction of a sealed stream: ChaCha20-Poly1305, nonce = 4 zero bytes || u64 BE counter.
pub struct Crypt {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl Crypt {
    fn new(key: [u8; 32]) -> Self {
        Crypt { cipher: ChaCha20Poly1305::new(&Key::from(key)), counter: 0 }
    }

    fn nonce(&mut self) -> Nonce {
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&self.counter.to_be_bytes());
        self.counter += 1;
        Nonce::from(n)
    }

    pub fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
        let n = self.nonce();
        self.cipher.encrypt(&n, plain).expect("encryption cannot fail")
    }

    pub fn open(&mut self, sealed: &[u8]) -> Result<Vec<u8>> {
        let n = self.nonce();
        self.cipher.decrypt(&n, sealed).map_err(|_| anyhow!("bad ciphertext"))
    }
}

/// `(client->server, server->client)` ciphers from the shared key and both nonces.
pub fn crypts(key: &[u8; 32], nonce_c: &[u8; 16], nonce_s: &[u8; 16]) -> (Crypt, Crypt) {
    let mut material = Vec::with_capacity(64);
    material.extend_from_slice(key);
    material.extend_from_slice(nonce_c);
    material.extend_from_slice(nonce_s);
    (
        Crypt::new(blake3::derive_key("nt v1 c2s", &material)),
        Crypt::new(blake3::derive_key("nt v1 s2c", &material)),
    )
}

/// The server closed the connection during the handshake: it does not know this client id.
#[derive(Debug)]
pub struct Rejected;

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the server rejected this client (unknown registration)")
    }
}

impl std::error::Error for Rejected {}

/// The daemon speaks a different protocol version (or closed the connection during the Hello, as older daemons
/// do). Not worth retrying: the message tells the user what to do.
#[derive(Debug)]
pub struct VersionMismatch(pub String);

impl std::fmt::Display for VersionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for VersionMismatch {}

/// TCP client side of the handshake: send `client_id || nonce_c`, read `nonce_s`, derive the ciphers.
pub async fn client_handshake(
    mut r: BoxRead,
    mut w: BoxWrite,
    client_id: &[u8; 16],
    key: &[u8; 32],
) -> Result<(FrameReader, FrameWriter)> {
    let mut nonce_c = [0u8; 16];
    getrandom::fill(&mut nonce_c).map_err(|e| anyhow!("getrandom: {e}"))?;
    let mut hello = [0u8; 32];
    hello[..16].copy_from_slice(client_id);
    hello[16..].copy_from_slice(&nonce_c);
    w.write_all(&hello).await?;
    w.flush().await?;
    let mut nonce_s = [0u8; 16];
    match r.read_exact(&mut nonce_s).await {
        Ok(_) => {}
        Err(e) if matches!(e.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset) => {
            return Err(Rejected.into());
        }
        Err(e) => return Err(e.into()),
    }
    let (c2s, s2c) = crypts(key, &nonce_c, &nonce_s);
    Ok((FrameReader::new(r, Some(s2c)), FrameWriter::new(w, Some(c2s))))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

pub type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// Framed message stream, either plain (unix socket) or sealed (TCP). All client/server logic is written
/// against these two types.
pub struct FrameReader {
    r: BoxRead,
    crypt: Option<Crypt>,
}

pub struct FrameWriter {
    w: BoxWrite,
    crypt: Option<Crypt>,
}

impl FrameWriter {
    pub fn new(w: BoxWrite, crypt: Option<Crypt>) -> Self {
        FrameWriter { w, crypt }
    }

    pub async fn write_msg<T: Serialize>(&mut self, msg: &T) -> Result<()> {
        let payload = postcard::to_allocvec(msg)?;
        if payload.len() > MAX_FRAME {
            bail!("frame too large ({} bytes)", payload.len());
        }
        let body = match &mut self.crypt {
            Some(c) => c.seal(&payload),
            None => payload,
        };
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&body);
        self.w.write_all(&frame).await?;
        self.w.flush().await?;
        Ok(())
    }
}

impl FrameReader {
    pub fn new(r: BoxRead, crypt: Option<Crypt>) -> Self {
        FrameReader { r, crypt }
    }

    /// Not cancel-safe: do not use inside `select!` (run it in its own task).
    pub async fn read_msg<T: DeserializeOwned>(&mut self) -> Result<T> {
        Ok(postcard::from_bytes(&self.read_payload().await?)?)
    }

    /// Read the first message of a connection. Only the protocol number is decoded here (see `hello_proto`), so
    /// a peer of any version gets a clean answer even when later fields of `Hello` or other messages have changed.
    /// Returns the protocol and, when it equals `PROTO_VERSION`, the full message.
    pub async fn read_hello(&mut self) -> Result<(u32, Option<C2S>)> {
        let plain = self.read_payload().await?;
        let proto = hello_proto(&plain).context("first message is not a Hello")?;
        let msg = if proto == PROTO_VERSION { Some(postcard::from_bytes(&plain)?) } else { None };
        Ok((proto, msg))
    }

    async fn read_payload(&mut self) -> Result<Vec<u8>> {
        let mut len = [0u8; 4];
        self.r.read_exact(&mut len).await?;
        let len = u32::from_be_bytes(len) as usize;
        if len > MAX_FRAME + TAG_LEN {
            bail!("frame too large ({len} bytes)");
        }
        let mut buf = vec![0u8; len];
        self.r.read_exact(&mut buf).await?;
        Ok(match &mut self.crypt {
            Some(c) => c.open(&buf)?,
            None => buf,
        })
    }
}

/// The protocol number of an encoded `C2S::Hello`. `Hello` must stay the first variant of `C2S` with `proto: u32`
/// as its first field: this prefix is all an older or newer peer can rely on.
pub fn hello_proto(payload: &[u8]) -> Option<u32> {
    let (variant, rest) = postcard::take_from_bytes::<u32>(payload).ok()?;
    if variant != 0 {
        return None;
    }
    postcard::take_from_bytes::<u32>(rest).ok().map(|(p, _)| p)
}

/// Escape codes that make a terminal show `screen` exactly: first reset the modes a previous screen may have
/// left on, then select the alternate or main screen, then paint contents, cursor visibility and input modes.
pub fn repaint(screen: &vt100::Screen) -> Vec<u8> {
    let mut v = b"\x1b[0m\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b[?1l\x1b>".to_vec();
    v.extend_from_slice(if screen.alternate_screen() { b"\x1b[?1049h" } else { b"\x1b[?1049l" });
    v.extend_from_slice(&screen.state_formatted());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repaint_restores_modes() {
        let mut p = vt100::Parser::new(24, 80, 0);
        p.process(b"\x1b[?1049h\x1b[?25l\x1b[?1h\x1b[?2004h\x1b[?1000h\x1b[?1006hvim");
        let s = String::from_utf8_lossy(&repaint(p.screen())).into_owned();
        for want in ["\x1b[?1049h", "\x1b[?25l", "\x1b[?1h", "\x1b[?2004h", "\x1b[?1000h", "\x1b[?1006h", "vim"] {
            assert!(s.contains(want), "missing {want:?} in {s:?}");
        }
        let mut q = vt100::Parser::new(24, 80, 0);
        q.process(&repaint(p.screen()));
        assert!(q.screen().alternate_screen() && q.screen().hide_cursor());
    }

    #[test]
    fn hello_prefix_is_stable() {
        let b = postcard::to_allocvec(&C2S::Hello { proto: 7, last_input_seq_acked: 123456 }).unwrap();
        assert_eq!(hello_proto(&b), Some(7));
        // Whatever follows the protocol number does not matter.
        let mut b = postcard::to_allocvec(&C2S::Hello { proto: 300, last_input_seq_acked: 0 }).unwrap();
        b.extend_from_slice(b"future fields");
        assert_eq!(hello_proto(&b), Some(300));
        assert_eq!(hello_proto(&postcard::to_allocvec(&C2S::List).unwrap()), None);
        assert_eq!(hello_proto(&[]), None);
    }

    #[test]
    fn hex_roundtrip() {
        let b = [1u8, 0xab, 0xff];
        assert_eq!(unhex::<3>(&hex(&b)), Some(b));
        assert_eq!(unhex::<3>("zz"), None);
    }

    #[test]
    fn crypt_roundtrip_and_tamper() {
        let (mut a, _) = crypts(&[1; 32], &[2; 16], &[3; 16]);
        let (mut b, _) = crypts(&[1; 32], &[2; 16], &[3; 16]);
        let mut ct = a.seal(b"hello");
        assert_eq!(b.open(&ct).unwrap(), b"hello");
        let ct2 = a.seal(b"x");
        ct[0] ^= 1;
        assert!(b.open(&ct2).is_ok());
        let (mut c, _) = crypts(&[1; 32], &[2; 16], &[3; 16]);
        assert!(c.open(&ct).is_err());
    }
}
