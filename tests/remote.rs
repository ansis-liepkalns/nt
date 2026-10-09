//! Remote (TCP, sealed) protocol tests against a real `nt server --foreground` on a random port.

#[allow(dead_code)]
#[path = "../src/proto.rs"]
mod proto;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use proto::{FrameReader, FrameWriter, SessionId, C2S, PROTO_VERSION, S2C};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};
use tokio::time::timeout;

struct Daemon {
    child: Child,
    dir: PathBuf,
    port: u16,
}

impl Daemon {
    fn start() -> Daemon {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("nt-rtest-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let child = Command::new(env!("CARGO_BIN_EXE_nt"))
            .args(["server", "--foreground", "--port", &port.to_string()])
            .env("XDG_RUNTIME_DIR", &dir)
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let d = Daemon { child, dir, port };
        let start = Instant::now();
        while std::os::unix::net::UnixStream::connect(d.sock()).is_err()
            || std::net::TcpStream::connect(("127.0.0.1", port)).is_err()
        {
            assert!(start.elapsed() < Duration::from_secs(10), "daemon did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        d
    }

    fn sock(&self) -> PathBuf {
        self.dir.join("nt/ctl.sock")
    }

    /// What `nt bootstrap` does: Register over the control socket.
    async fn register(&self) -> ([u8; 16], [u8; 32]) {
        let mut c = self.local().await;
        c.send(&C2S::Register).await;
        match c.recv().await {
            S2C::Registered { port, client_id, key } => {
                assert_eq!(port, self.port);
                (client_id, key)
            }
            m => panic!("unexpected {m:?}"),
        }
    }

    async fn local(&self) -> Conn {
        let (r, w) = UnixStream::connect(self.sock()).await.unwrap().into_split();
        let mut c = Conn::new(FrameReader::new(Box::new(r), None), FrameWriter::new(Box::new(w), None));
        c.hello().await;
        c
    }

    async fn remote(&self, id: &[u8; 16], key: &[u8; 32]) -> Conn {
        let s = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let (r, w) = s.into_split();
        let (r, w) = proto::client_handshake(Box::new(r), Box::new(w), id, key).await.unwrap();
        let mut c = Conn::new(r, w);
        c.hello().await;
        c
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Conn {
    r: FrameReader,
    w: FrameWriter,
    welcome_seq: u64,
    /// End offset of the output received so far.
    off: u64,
}

impl Conn {
    fn new(r: FrameReader, w: FrameWriter) -> Conn {
        Conn { r, w, welcome_seq: 0, off: 0 }
    }

    async fn hello(&mut self) {
        self.send(&C2S::Hello { proto: PROTO_VERSION, last_input_seq_acked: 0 }).await;
        match self.recv().await {
            S2C::Welcome { last_input_seq, .. } => self.welcome_seq = last_input_seq,
            m => panic!("expected Welcome, got {m:?}"),
        }
    }

    async fn send(&mut self, m: &C2S) {
        self.w.write_msg(m).await.unwrap();
    }

    async fn recv(&mut self) -> S2C {
        timeout(Duration::from_secs(10), self.r.read_msg::<S2C>()).await.expect("timed out").expect("read failed")
    }

    async fn attach(&mut self, session: Option<SessionId>, resume_from: Option<u64>) -> SessionId {
        self.send(&C2S::Attach { session, cols: 80, rows: 24, term: "xterm".into(), resume_from , cwd: None }).await;
        loop {
            if let S2C::Attached { session } = self.recv().await {
                return session;
            }
        }
    }

    async fn input(&mut self, seq: u64, text: &str) {
        self.send(&C2S::Input { seq, data: text.as_bytes().to_vec() }).await;
    }

    /// Collect output until it contains `needle`; panics on a Redraw if `no_redraw`.
    async fn wait(&mut self, needle: &str, no_redraw: bool) -> String {
        let mut acc = String::new();
        while !acc.contains(needle) {
            match self.recv().await {
                S2C::Output { offset, data } => {
                    self.off = offset + data.len() as u64;
                    acc.push_str(&String::from_utf8_lossy(&data));
                }
                S2C::Redraw { offset, data } => {
                    assert!(!no_redraw, "unexpected redraw");
                    self.off = offset;
                    acc.push_str(&String::from_utf8_lossy(&data));
                }
                S2C::SessionEnded { .. } => panic!("session ended; got {acc:?}"),
                _ => {}
            }
        }
        acc
    }
}

#[tokio::test(flavor = "current_thread")]
async fn handshake_attach_echo() {
    let d = Daemon::start();
    let (id, key) = d.register().await;
    let mut c = d.remote(&id, &key).await;
    assert_eq!(c.welcome_seq, 0);
    let sid = c.attach(None, None).await;
    c.input(1, "echo hi$((40+2))\n").await;
    c.wait("hi42", false).await;
    c.send(&C2S::Ping).await;
    loop {
        if let S2C::Pong = c.recv().await {
            break;
        }
    }
    // Register is refused over TCP.
    c.send(&C2S::Register).await;
    loop {
        if let S2C::Error(_) = c.recv().await {
            break;
        }
    }
    // The same session is visible from the local socket.
    let mut l = d.local().await;
    l.send(&C2S::List).await;
    loop {
        if let S2C::Sessions(v) = l.recv().await
            && !v.is_empty()
        {
            assert_eq!(v[0].id, sid);
            break;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn wrong_key_and_unknown_id_rejected() {
    let d = Daemon::start();
    let (id, key) = d.register().await;

    // Wrong key: the handshake itself completes, but the sealed Hello cannot be opened and the server hangs up.
    let s = TcpStream::connect(("127.0.0.1", d.port)).await.unwrap();
    let (r, w) = s.into_split();
    let mut bad = key;
    bad[0] ^= 0xff;
    let (r, w) = proto::client_handshake(Box::new(r), Box::new(w), &id, &bad).await.unwrap();
    let mut c = Conn::new(r, w);
    c.send(&C2S::Hello { proto: PROTO_VERSION, last_input_seq_acked: 0 }).await;
    assert!(timeout(Duration::from_secs(10), c.r.read_msg::<S2C>()).await.unwrap().is_err());

    // Unknown client id: closed before the server nonce is sent.
    let s = TcpStream::connect(("127.0.0.1", d.port)).await.unwrap();
    let (r, w) = s.into_split();
    let err = match proto::client_handshake(Box::new(r), Box::new(w), &[7; 16], &key).await {
        Ok(_) => panic!("unknown id accepted"),
        Err(e) => e,
    };
    assert!(err.is::<proto::Rejected>(), "{err:#}");

    // The good key still works afterwards.
    let _ = d.remote(&id, &key).await;
}

#[tokio::test(flavor = "current_thread")]
async fn reconnect_resumes_missed_output() {
    let d = Daemon::start();
    let (id, key) = d.register().await;
    let mut a = d.remote(&id, &key).await;
    let sid = a.attach(None, None).await;
    a.input(1, "echo first$((1+1))\n").await;
    a.wait("first2", false).await;
    let off = a.off;
    drop(a); // network drop

    // Output produced while the remote client is away.
    let mut l = d.local().await;
    l.attach(Some(sid), None).await;
    l.input(1, "echo missed$((3+4))\n").await;
    l.wait("missed7", false).await;

    let mut b = d.remote(&id, &key).await;
    b.attach(Some(sid), Some(off)).await;
    let got = b.wait("missed7", true).await;
    assert!(!got.contains("first2"), "replay must start at the resume offset: {got:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn input_dedup_across_reconnect() {
    let d = Daemon::start();
    let (id, key) = d.register().await;
    let mut a = d.remote(&id, &key).await;
    let sid = a.attach(None, None).await;
    a.input(1, "echo n$((20+22))\n").await;
    a.wait("n42", false).await;
    let off = a.off;
    drop(a);

    let mut b = d.remote(&id, &key).await;
    assert_eq!(b.welcome_seq, 1, "server remembers the last applied seq for this client id");
    b.attach(Some(sid), Some(off)).await;
    b.input(1, "echo n$((20+22))\n").await; // resent unacked input: must be dropped
    b.input(2, "echo done$((1+1))\n").await;
    let got = b.wait("done2", true).await;
    assert!(!got.contains("n42"), "duplicate input was applied: {got:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn tampered_ciphertext_drops_connection() {
    let d = Daemon::start();
    let (id, key) = d.register().await;
    let mut s = TcpStream::connect(("127.0.0.1", d.port)).await.unwrap();
    let nonce_c = [9u8; 16];
    let mut hello = id.to_vec();
    hello.extend_from_slice(&nonce_c);
    s.write_all(&hello).await.unwrap();
    let mut nonce_s = [0u8; 16];
    s.read_exact(&mut nonce_s).await.unwrap();
    let (mut c2s, s2c) = proto::crypts(&key, &nonce_c, &nonce_s);

    let frame = |body: Vec<u8>| {
        let mut f = (body.len() as u32).to_be_bytes().to_vec();
        f.extend(body);
        f
    };
    let msg = C2S::Hello { proto: PROTO_VERSION, last_input_seq_acked: 0 };
    s.write_all(&frame(c2s.seal(&postcard::to_allocvec(&msg).unwrap()))).await.unwrap();
    let (r, mut w) = s.into_split();
    let mut r = FrameReader::new(Box::new(r), Some(s2c));
    assert!(matches!(timeout(Duration::from_secs(10), r.read_msg::<S2C>()).await.unwrap().unwrap(), S2C::Welcome { .. }));

    let mut sealed = c2s.seal(&postcard::to_allocvec(&C2S::Ping).unwrap());
    sealed[0] ^= 1;
    w.write_all(&frame(sealed)).await.unwrap();
    assert!(timeout(Duration::from_secs(10), r.read_msg::<S2C>()).await.unwrap().is_err(), "server should hang up");
}
