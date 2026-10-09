//! Talks the wire protocol directly to a real `nt server --foreground` over its unix socket.

#[allow(dead_code)]
#[path = "../src/proto.rs"]
mod proto;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use proto::{NameSource, SessionId, SessionInfo, C2S, PROTO_VERSION, S2C};

struct Daemon {
    child: Child,
    dir: PathBuf,
}

impl Daemon {
    fn start() -> Daemon {
        Daemon::start_with(&[], "/bin/sh")
    }

    fn start_with(env: &[(&str, String)], shell: &str) -> Daemon {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("nt-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home")).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_nt"))
            .args(["server", "--foreground", "--port", "0"])
            .env("XDG_RUNTIME_DIR", &dir)
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("SHELL", shell)
            .env("HOME", dir.join("home"))
            .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let d = Daemon { child, dir };
        let start = Instant::now();
        while UnixStream::connect(d.sock()).is_err() {
            assert!(start.elapsed() < Duration::from_secs(10), "daemon did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        d
    }

    fn sock(&self) -> PathBuf {
        self.dir.join("nt/ctl.sock")
    }

    fn connect(&self) -> Conn {
        let s = UnixStream::connect(self.sock()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut c = Conn { s, seq: 0 };
        c.send(&C2S::Hello { proto: PROTO_VERSION, last_input_seq_acked: 0 });
        assert!(matches!(c.recv(), S2C::Welcome { proto: PROTO_VERSION, .. }));
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
    s: UnixStream,
    seq: u64,
}

impl Conn {
    fn send(&mut self, m: &C2S) {
        let p = postcard::to_allocvec(m).unwrap();
        self.s.write_all(&(p.len() as u32).to_be_bytes()).unwrap();
        self.s.write_all(&p).unwrap();
    }

    fn recv(&mut self) -> S2C {
        let mut len = [0u8; 4];
        self.s.read_exact(&mut len).expect("timed out waiting for server");
        let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
        self.s.read_exact(&mut buf).unwrap();
        postcard::from_bytes(&buf).unwrap()
    }

    fn input(&mut self, text: &str) {
        self.seq += 1;
        let m = C2S::Input { seq: self.seq, data: text.as_bytes().to_vec() };
        self.send(&m);
    }

    fn list(&mut self) -> Vec<SessionInfo> {
        self.send(&C2S::List);
        self.next_sessions()
    }

    /// Next `Sessions` message, skipping anything else.
    fn next_sessions(&mut self) -> Vec<SessionInfo> {
        loop {
            if let S2C::Sessions(v) = self.recv() {
                return v;
            }
        }
    }

    fn attach(&mut self, session: Option<SessionId>, resume_from: Option<u64>) -> SessionId {
        self.send(&C2S::Attach { session, cols: 80, rows: 24, term: "xterm".into(), resume_from , cwd: None });
        loop {
            if let S2C::Attached { session } = self.recv() {
                return session;
            }
        }
    }

    /// Collect Output until the accumulated text contains `needle`.
    fn wait_output(&mut self, needle: &str) -> String {
        let mut acc = String::new();
        while !acc.contains(needle) {
            match self.recv() {
                S2C::Output { data, .. } | S2C::Redraw { data, .. } => {
                    acc.push_str(&String::from_utf8_lossy(&data))
                }
                S2C::SessionEnded { .. } => panic!("session ended while waiting for {needle:?}: {acc:?}"),
                _ => {}
            }
        }
        acc
    }

    fn wait_ended(&mut self, id: SessionId) {
        loop {
            if let S2C::SessionEnded { session } = self.recv() {
                assert_eq!(session, id);
                return;
            }
        }
    }
}

#[test]
fn full_session_lifecycle() {
    let d = Daemon::start();
    let mut c1 = d.connect();
    assert!(c1.list().is_empty());

    // Attach new, run a command.
    let id = c1.attach(None, None);
    c1.input("echo hi\n");
    c1.wait_output("hi");
    c1.input("echo ab$((1+2))cd\n");
    c1.wait_output("ab3cd");

    // List shows one session with an adjective-noun name.
    let list = c1.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, id);
    assert_eq!(list[0].clients, 1);
    assert_eq!(list[0].name_source, NameSource::Initial);
    assert!(list[0].name.contains('-'), "name = {}", list[0].name);

    // A second client attaches and gets a repaint containing the earlier output.
    let mut c2 = d.connect();
    assert_eq!(c2.attach(Some(id), None), id);
    let redraw = loop {
        if let S2C::Redraw { data, .. } = c2.recv() {
            break String::from_utf8_lossy(&data).into_owned();
        }
    };
    assert!(redraw.contains("hi"), "redraw = {redraw:?}");

    // Both clients see new output.
    c1.input("echo shared\n");
    c1.wait_output("shared");
    c2.wait_output("shared");

    // Resume from an offset still in the ring replays output instead of redrawing.
    let mut c3 = d.connect();
    c3.send(&C2S::Attach { session: Some(id), cols: 80, rows: 24, term: "xterm".into(), resume_from: Some(0) , cwd: None });
    loop {
        match c3.recv() {
            S2C::Redraw { .. } => panic!("expected replay, got redraw"),
            S2C::Output { offset, data } => {
                assert_eq!(offset, 0);
                assert!(String::from_utf8_lossy(&data).len() > 2);
                break;
            }
            _ => {}
        }
    }
    drop(c3);

    // Rename (user-locked), then back to automatic.
    c2.send(&C2S::Rename { session: id, name: "my session".into() });
    let l = loop {
        let l = c1.next_sessions();
        if l[0].name == "my session" {
            break l;
        }
    };
    assert_eq!(l[0].name_source, NameSource::User);
    c1.send(&C2S::Rename { session: id, name: String::new() });
    let l = loop {
        let l = c1.next_sessions();
        if l[0].name != "my session" {
            break l;
        }
    };
    assert_eq!(l[0].name_source, NameSource::Initial);

    // Attaching to an unknown session is an error.
    c2.send(&C2S::Attach { session: Some(999), cols: 80, rows: 24, term: "x".into(), resume_from: None , cwd: None });
    loop {
        if let S2C::Error(_) = c2.recv() {
            break;
        }
    }

    // Kill ends the session for everyone.
    c1.send(&C2S::Kill { session: id });
    c1.wait_ended(id);
    c2.wait_ended(id);
    assert!(c1.list().is_empty());
}

#[test]
fn shell_exit_ends_session_and_ping() {
    let d = Daemon::start();
    let mut c = d.connect();
    c.send(&C2S::Ping);
    assert!(matches!(c.recv(), S2C::Ack { last_input_seq: 0 }));
    assert!(matches!(c.recv(), S2C::Pong));
    let id = c.attach(None, None);
    c.input("cd /tmp; echo now-in-tmp\n");
    c.wait_output("now-in-tmp\r\n$");
    c.input("exit\n");
    // The session survives its shell: it is listed as exited, with the screen kept.
    let l = loop {
        let l = c.next_sessions();
        if l[0].exited {
            break l;
        }
    };
    assert_eq!((l.len(), l[0].id, l[0].cwd.as_str()), (1, id, "/tmp"));
    c.input("echo ignored\n"); // nobody is listening
    c.send(&C2S::Respawn { session: id });
    let l = loop {
        let l = c.next_sessions();
        if !l[0].exited {
            break l;
        }
    };
    assert_eq!((l.len(), l[0].id), (1, id));
    c.input("pwd; echo back$((20+1))\n");
    let out = c.wait_output("back21");
    assert!(out.contains("/tmp") && !out.contains("ignored"), "{out:?}");

    // Exit again; closing an exited session ends it for good.
    c.input("exit\n");
    loop {
        if c.next_sessions()[0].exited {
            break;
        }
    }
    c.send(&C2S::Kill { session: id });
    c.wait_ended(id);
    assert!(c.list().is_empty());
}

#[test]
fn duplicate_input_is_dropped() {
    let d = Daemon::start();
    let mut c = d.connect();
    c.attach(None, None);
    c.input("echo one\n"); // seq 1
    c.send(&C2S::Input { seq: 1, data: b"echo dup\n".to_vec() }); // replayed seq: ignored
    c.input("echo two\n");
    let out = c.wait_output("two");
    assert!(!out.contains("dup"), "{out:?}");
}

#[test]
fn output_goes_to_transcript_and_search_finds_it() {
    let d = Daemon::start();
    let mut c = d.connect();
    let id = c.attach(None, None);
    c.input("echo zq$((6*7))Marker\n");
    c.wait_output("zq42Marker");
    c.input("printf 'progress 10%%\\rprogress 99%%\\n'\n");
    c.wait_output("progress 99%");

    // Case-insensitive substring search over the transcript; hits are for this session.
    let hits = loop {
        c.send(&C2S::Search { query: "ZQ42marker".into() });
        let hits = loop {
            if let S2C::SearchResults(h) = c.recv() {
                break h;
            }
        };
        if !hits.is_empty() {
            break hits;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(hits[0].session, id);
    assert!(hits[0].line.ends_with("zq42Marker"), "{:?}", hits[0].line);
    c.send(&C2S::Search { query: "no-such-text-anywhere".into() });
    loop {
        if let S2C::SearchResults(h) = c.recv() {
            assert!(h.is_empty());
            break;
        }
    }

    // The file itself: plain text, \r overwrite applied, and a .meta next to it.
    let tdir = d.dir.join("state/nt/transcripts");
    let txt = std::fs::read_dir(&tdir).unwrap().map(|e| e.unwrap().path()).find(|p| p.extension().is_some_and(|e| e == "txt")).unwrap();
    let text = std::fs::read_to_string(&txt).unwrap();
    assert!(text.lines().any(|l| l.ends_with("zq42Marker")), "{text:?}");
    assert!(text.lines().any(|l| l.ends_with("progress 99%")), "{text:?}");
    let meta = std::fs::read_to_string(txt.with_extension("meta")).unwrap();
    assert!(meta.starts_with("name=") && meta.contains("name_source=Initial"), "{meta:?}");
}

/// A client with another protocol version, even one whose Hello has more fields, is told so before the close.
#[test]
fn version_mismatch_gets_an_error() {
    let d = Daemon::start();
    for extra in [&b""[..], &b"\x01\x02\x03 fields from a future version"[..]] {
        let mut s = UnixStream::connect(d.sock()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut p = postcard::to_allocvec(&C2S::Hello { proto: PROTO_VERSION + 5, last_input_seq_acked: 0 }).unwrap();
        p.extend_from_slice(extra);
        s.write_all(&(p.len() as u32).to_be_bytes()).unwrap();
        s.write_all(&p).unwrap();
        let mut c = Conn { s, seq: 0 };
        match c.recv() {
            S2C::Error(e) => {
                assert!(e.contains("version mismatch") && e.contains("0 sessions") && e.contains("nt server --restart"), "{e}");
            }
            m => panic!("expected Error, got {m:?}"),
        }
        let mut rest = Vec::new();
        assert_eq!(c.s.read_to_end(&mut rest).unwrap(), 0, "connection should be closed");
    }
    // A matching client still works afterwards.
    d.connect();
}

/// `nt bootstrap` against a daemon that just hangs up (an older version) explains what to do.
#[test]
fn bootstrap_reports_incompatible_daemon() {
    let dir = std::env::temp_dir().join(format!("nt-test-old-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("nt")).unwrap();
    let l = std::os::unix::net::UnixListener::bind(dir.join("nt/ctl.sock")).unwrap();
    std::thread::spawn(move || {
        for s in l.incoming() {
            drop(s);
        }
    });
    let out = Command::new(env!("CARGO_BIN_EXE_nt")).arg("bootstrap").env("XDG_RUNTIME_DIR", &dir).output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("nt server --restart") && err.contains("different nt version"), "{err}");
}

/// `nt server --restart` replaces a running daemon.
#[test]
fn restart_replaces_daemon() {
    let mut d = Daemon::start();
    let mut c = d.connect();
    c.attach(None, None);
    let mut new = Command::new(env!("CARGO_BIN_EXE_nt"))
        .args(["server", "--foreground", "--restart", "--port", "0"])
        .env("XDG_RUNTIME_DIR", &d.dir)
        .env("XDG_STATE_HOME", d.dir.join("state"))
        .env("SHELL", "/bin/sh")
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    assert!(d.child.wait().is_ok()); // the old one exited on SIGTERM
    let start = Instant::now();
    let mut ok = false;
    while start.elapsed() < Duration::from_secs(10) {
        if let Ok(s) = UnixStream::connect(d.sock()) {
            drop(s);
            ok = d.connect().list().is_empty();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = new.kill();
    let _ = new.wait();
    assert!(ok, "new daemon did not come up empty");
}

/// Highest VmHWM (KiB) of any `nt namer` process right now.
fn namer_hwm_kb() -> u64 {
    let mut best = 0;
    for e in std::fs::read_dir("/proc").unwrap().flatten() {
        let p = e.path();
        let Ok(cmd) = std::fs::read(p.join("cmdline")) else { continue };
        let cmd = String::from_utf8_lossy(&cmd).replace('\0', " ");
        if cmd.contains(" namer --model")
            && let Ok(st) = std::fs::read_to_string(p.join("status"))
            && let Some(l) = st.lines().find(|l| l.starts_with("VmHWM"))
        {
            best = best.max(l.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0));
        }
    }
    best
}

/// Real model end to end (slow, needs a GGUF): `NT_E2E_MODEL=/path/model.gguf NT_E2E_CACHE=/tmp/dir
/// cargo test --test daemon e2e_namer -- --ignored --nocapture`. Run it twice with the same cache dir for cold/warm numbers.
#[test]
#[ignore]
fn e2e_namer() {
    let model = std::env::var("NT_E2E_MODEL").expect("NT_E2E_MODEL");
    let cache = PathBuf::from(std::env::var("NT_E2E_CACHE").expect("NT_E2E_CACHE"));
    std::fs::create_dir_all(&cache).unwrap();
    let cfgdir = cache.join("config");
    std::fs::create_dir_all(cfgdir.join("nt")).unwrap();
    std::fs::write(cfgdir.join("nt/config.toml"), format!("llm_model = \"{model}\"\nllm_on_battery = true\n")).unwrap();
    let env = [
        ("XDG_CACHE_HOME", cache.join("cache").display().to_string()),
        ("XDG_CONFIG_HOME", cfgdir.display().to_string()),
        ("XDG_DATA_HOME", cache.join("data").display().to_string()),
        ("HOME", cache.display().to_string()),
    ];
    let d = Daemon::start_with(&env, "/usr/bin/bash");
    let mut c = d.connect();
    let id = c.attach(None, None);
    let mut c2 = d.connect();
    let id2 = c2.attach(None, None);
    for cmd in [
        "cd /usr/share && ls | head -3\n",
        "cargo --version\n",
        "grep -r needle /etc/hostname\n",
        "ls /nonexistent\n",
        "echo done-marker\n",
    ] {
        c.input(cmd);
        c2.input(cmd.replace("done-marker", "done-marker-2").replace("cargo --version", "rustc --version").as_str());
        std::thread::sleep(Duration::from_millis(700));
    }
    c.wait_output("done-marker");
    c2.wait_output("done-marker-2");
    std::thread::sleep(Duration::from_millis(1000));
    // The watching client is attached to the second session: it must be named first.
    let mut w = d.connect();
    w.attach(Some(id2), None);
    w.send(&C2S::Watch(true));
    let t0 = Instant::now();
    let (mut saw_pending, mut peak) = (false, 0);
    let mut done: Vec<(SessionId, Duration)> = Vec::new();
    w.s.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    while t0.elapsed() < Duration::from_secs(300) && done.len() < 2 {
        peak = peak.max(namer_hwm_kb());
        let mut len = [0u8; 4];
        if w.s.read_exact(&mut len).is_err() {
            continue;
        }
        let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
        w.s.read_exact(&mut buf).unwrap();
        if let S2C::Sessions(v) = postcard::from_bytes(&buf).unwrap() {
            for s in v.iter().filter(|s| (s.id == id || s.id == id2) && !s.recap.is_empty() && !s.pending) {
                if !done.iter().any(|d| d.0 == s.id) {
                    done.push((s.id, t0.elapsed()));
                    eprintln!("E2E session {} at {:?}: topic={:?} summary={:?} name={:?}", s.id, t0.elapsed(), s.recap, s.summary, s.name);
                }
            }
            saw_pending |= v.iter().any(|s| s.pending);
        }
    }
    peak = peak.max(namer_hwm_kb());
    eprintln!("E2E results={done:?} pending_seen={saw_pending} namer_peak_rss_kib={peak}");
    assert!(done.len() == 2 && saw_pending);
    assert_eq!(done[0].0, id2, "the watcher's current session is named first");
}

/// A new session starts in the directory the client asks for; a missing or bad one falls back to `$HOME`.
#[test]
fn new_session_starts_in_requested_cwd() {
    let d = Daemon::start();
    let want = d.dir.join("work dir");
    std::fs::create_dir_all(&want).unwrap();
    let home = d.dir.join("home");
    for (cwd, expect, marker) in [
        (Some(want.to_str().unwrap().to_string()), want.clone(), "A"),
        (Some("/no/such/dir".to_string()), home.clone(), "B"),
        (Some("relative".to_string()), home.clone(), "C"),
        (None, home.clone(), "D"),
    ] {
        let mut c = d.connect();
        c.send(&C2S::Attach { session: None, cols: 80, rows: 24, term: "xterm".into(), resume_from: None, cwd });
        loop {
            match c.recv() {
                S2C::Attached { .. } => break,
                S2C::Error(e) => panic!("attach failed: {e}"),
                _ => {}
            }
        }
        c.input("echo @@$(pwd -P)@@\r");
        let canon = std::fs::canonicalize(&expect).unwrap();
        c.wait_output(&format!("@@{}@@", canon.display()));
        let _ = marker;
    }
}
