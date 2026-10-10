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
fn kill_reaches_jobs_in_their_own_process_group() {
    // Like `docker exec -it` in an interactive shell: a foreground job in its own process group that
    // survives SIGHUP. Closing the session must still end it and the session.
    let d = Daemon::start();
    let mut c = d.connect();
    let id = c.attach(None, None);
    // `job$((1))` only expands in the output, not in the echoed command line.
    c.input("bash -c 'trap \"\" HUP; echo job$((1))pid=$$:end$((2)); exec sleep 600'\n");
    let out = c.wait_output(":end2");
    let pid: i32 = out
        .rsplit("job1pid=")
        .next()
        .and_then(|r| r.split(':').next())
        .and_then(|n| n.parse().ok())
        .expect("job pid");
    c.send(&C2S::Kill { session: id });
    c.wait_ended(id);
    // The job got SIGKILL with the rest of the session (allow a moment for the kernel to reap it).
    let gone = (0..50).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(100));
        std::fs::read_to_string(format!("/proc/{pid}/stat")).map(|s| s.contains(") Z ")).unwrap_or(true)
    });
    assert!(gone, "job {pid} survived the kill");
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

// ---------------------------------------------------------------- live numbers, branch and stale summaries

impl Conn {
    /// Read pushes until `f` accepts one; fails after `secs`. Returns the accepted list and the gaps between pushes seen.
    fn until(&mut self, secs: u64, mut f: impl FnMut(&[SessionInfo]) -> bool) -> (Vec<SessionInfo>, Vec<Duration>) {
        let t0 = Instant::now();
        let (mut last, mut gaps) = (Instant::now(), Vec::new());
        while t0.elapsed() < Duration::from_secs(secs) {
            if let S2C::Sessions(v) = self.recv() {
                gaps.push(last.elapsed());
                last = Instant::now();
                if f(&v) {
                    return (v, gaps);
                }
            }
        }
        panic!("condition not reached within {secs}s");
    }
}

fn session(v: &[SessionInfo], id: SessionId) -> &SessionInfo {
    v.iter().find(|s| s.id == id).expect("session in list")
}

/// A burner started in a session shows up in the unsolicited pushes of a watching client within a few seconds,
/// and disappears again after it is killed; the pushes keep coming every ~2 s while the switcher is open.
#[test]
fn cpu_and_memory_pushes_follow_a_burner_while_watching() {
    let d = Daemon::start_with(&[], "/usr/bin/bash");
    let mut c = d.connect();
    let id = c.attach(None, None);
    c.input("echo ready-$((1+1))\n");
    c.wait_output("ready-2");
    let mut w = d.connect();
    w.send(&C2S::Watch(true));
    // idle first
    let (v, _) = w.until(8, |v| v.iter().any(|s| s.id == id && s.rss_bytes > 0));
    assert!(session(&v, id).cpu_pct < 30.0, "idle cpu {}", session(&v, id).cpu_pct);
    for round in 0..2 {
        c.input("yes >/dev/null &\n");
        let (v, gaps) = w.until(12, |v| session(v, id).cpu_pct > 50.0);
        assert!(session(&v, id).rss_bytes > 100_000);
        assert!(gaps.iter().all(|g| *g < Duration::from_millis(3500)), "round {round}: pushes too far apart: {gaps:?}");
        c.input("kill $!\n");
        let (v, _) = w.until(12, |v| session(v, id).cpu_pct < 20.0);
        assert!(session(&v, id).cpu_pct < 20.0);
    }
}

/// A switcher opened after a long idle time gets fresh numbers at once: memory immediately, CPU within a second.
#[test]
fn opening_the_switcher_refreshes_idle_sampled_numbers() {
    let d = Daemon::start_with(&[], "/usr/bin/bash");
    let mut c = d.connect();
    let id = c.attach(None, None);
    c.input("yes >/dev/null &\n");
    std::thread::sleep(Duration::from_millis(700));
    let mut w = d.connect();
    w.send(&C2S::Watch(true));
    // the first push after Watch(true) has memory; a CPU figure follows well before the regular 2 s period
    let t0 = Instant::now();
    let (v, _) = w.until(6, |v| session(v, id).cpu_pct > 50.0);
    assert!(session(&v, id).rss_bytes > 100_000);
    assert!(t0.elapsed() < Duration::from_millis(2500), "took {:?}", t0.elapsed());
    c.input("kill $!\n");
}

/// A tiny OpenAI-compatible server on localhost: records every request body and answers with `reply(n, body)`.
fn fake_llm(reply: impl Fn(usize, &str) -> String + Send + 'static) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = bodies.clone();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let (head_end, len) = loop {
                let n = s.read(&mut tmp).unwrap_or(0);
                if n == 0 {
                    break (0, 0);
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                    let len = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
                    break (p + 4, len);
                }
            };
            while head_end > 0 && buf.len() < head_end + len {
                let n = s.read(&mut tmp).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let body = String::from_utf8_lossy(&buf[head_end.min(buf.len())..]).into_owned();
            let n = {
                let mut g = seen.lock().unwrap();
                g.push(body.clone());
                g.len() - 1
            };
            let content = reply(n, &body).replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
            let json = format!("{{\"choices\":[{{\"message\":{{\"role\":\"assistant\",\"content\":\"{content}\"}}}}]}}");
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}", json.len());
        }
    });
    (port, bodies)
}

fn api_env(port: u16, tag: &str) -> Vec<(&'static str, String)> {
    let cfg = std::env::temp_dir().join(format!("nt-test-cfg-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(cfg.join("nt")).unwrap();
    std::fs::write(cfg.join("nt/config.toml"), format!("llm = \"api\"\nllm_api_url = \"http://127.0.0.1:{port}/v1/chat/completions\"\nllm_api_key_env = \"NT_TEST_KEY\"\n")).unwrap();
    vec![("XDG_CONFIG_HOME", cfg.display().to_string()), ("NT_TEST_KEY", "k".into())]
}

/// A build that was started, summarized while running and then interrupted: the summary is flagged as older than the
/// last command at once, opening the switcher shows it pending and refreshes it with the interrupted command in the digest,
/// and a sentence that still claims the build runs is dropped from the new summary.
#[test]
fn interrupted_build_marks_the_summary_stale_and_opening_refreshes_it() {
    let (port, bodies) = fake_llm(|n, _| {
        if n == 0 {
            "name: build-session\ntopic: building the project\nsummary: Started a long build in the home directory. The build is currently running for 3 minutes.".to_string()
        } else {
            "name: build-session\ntopic: build was interrupted\nsummary: Started a long build and stopped it with Ctrl-C. The build is still running. No further commands were run.".to_string()
        }
    });
    let env = api_env(port, "stale");
    let d = Daemon::start_with(&env, "/usr/bin/bash");
    let mut c = d.connect();
    let id = c.attach(None, None);
    c.input("echo ready-$((1+1))\n");
    c.wait_output("ready-2");
    c.input("sleep 300\n");
    std::thread::sleep(Duration::from_millis(500));
    let mut w = d.connect();
    w.attach(Some(id), None);
    w.send(&C2S::Watch(true));
    let (v, _) = w.until(30, |v| !session(v, id).summary.is_empty() && !session(v, id).pending);
    let s = session(&v, id);
    assert!(s.summary.contains("currently running"), "a running command keeps its summary: {}", s.summary);
    assert!(!s.summary_stale && s.summary_unix > 0, "fresh while the build still runs");
    // Ctrl-C: the command ends (OSC 133 D with 130); the summary now predates the last command end.
    c.input("\x03");
    let (v, _) = w.until(10, |v| session(v, id).summary_stale);
    assert!(session(&v, id).status.starts_with('✗'), "status {}", session(&v, id).status);
    // Close and open the switcher: pending first, then a refreshed, no longer stale summary.
    w.send(&C2S::Watch(false));
    std::thread::sleep(Duration::from_millis(300));
    let t0 = Instant::now();
    w.send(&C2S::Watch(true));
    let (_, _) = w.until(10, |v| session(v, id).pending);
    let (v, _) = w.until(30, |v| !session(v, id).pending && session(v, id).summary.contains("Ctrl-C"));
    let s = session(&v, id);
    assert!(t0.elapsed() < Duration::from_secs(5), "took {:?}", t0.elapsed());
    assert!(!s.summary_stale, "refreshed");
    assert!(!s.summary.contains("still running") && s.summary.contains("No further commands"), "live sentence dropped: {}", s.summary);
    let second = bodies.lock().unwrap()[1].clone();
    assert!(second.contains("exit 130") && second.contains("sleep 300"), "digest lacks the interrupted command: {second}");
    assert!(second.contains("earlier summary: Started a long build in the home directory.") && !second.contains("for 3 minutes"), "the old summary's live claim must not be fed back: {second}");
}

/// A `git checkout` (here: .git/HEAD rewritten, also through a worktree-style `.git` file) changes the branch shown, marks
/// the summary stale without any command, and the next refresh tells the model about the branch change.
#[test]
fn branch_change_is_pushed_and_triggers_a_new_summary() {
    let (port, bodies) = fake_llm(|n, _| format!("name: repo-work\ntopic: working on topic number {n}\nsummary: Looked around the repository and listed its files."));
    let env = api_env(port, "branch");
    let d = Daemon::start_with(&env, "/usr/bin/bash");
    let repo = d.dir.join("home/shop");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    // a worktree: `.git` is a file pointing at a gitdir
    let wt = d.dir.join("home/shop-wt");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::create_dir_all(d.dir.join("wtgit")).unwrap();
    std::fs::write(d.dir.join("wtgit/HEAD"), "ref: refs/heads/hotfix/1\n").unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", d.dir.join("wtgit").display())).unwrap();
    let mk = |dir: &std::path::Path| {
        let mut c = d.connect();
        c.send(&C2S::Attach { session: None, cols: 80, rows: 24, term: "xterm".into(), resume_from: None, cwd: Some(dir.display().to_string()) });
        let id = loop {
            if let S2C::Attached { session } = c.recv() {
                break session;
            }
        };
        c.input("ls -la\n");
        (c, id)
    };
    let (mut c1, id1) = mk(&repo);
    let (_c2, id2) = mk(&wt);
    c1.wait_output("total");
    std::thread::sleep(Duration::from_millis(500));
    let mut w = d.connect();
    w.attach(Some(id1), None);
    w.send(&C2S::Watch(true));
    let (v, _) = w.until(30, |v| !session(v, id1).summary.is_empty() && !session(v, id2).summary.is_empty() && !v.iter().any(|s| s.pending));
    assert_eq!((session(&v, id1).branch.as_str(), session(&v, id2).branch.as_str()), ("main", "hotfix/1"));
    assert!(!session(&v, id1).summary_stale);
    let n_before = bodies.lock().unwrap().len();
    // switch branches behind the shell's back; the watching client is told within a couple of sampling periods
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feature/coupons\n").unwrap();
    let (v, _) = w.until(8, |v| session(v, id1).branch == "feature/coupons");
    assert!(session(&v, id1).summary_stale && session(&v, id2).branch == "hotfix/1");
    // reopening refreshes the session even though no command ran, and the digest says what changed
    w.send(&C2S::Watch(false));
    std::thread::sleep(Duration::from_millis(200));
    w.send(&C2S::Watch(true));
    let (v, _) = w.until(30, |v| !session(v, id1).summary_stale && !v.iter().any(|s| s.pending) && bodies.lock().unwrap().len() > n_before);
    assert!(!session(&v, id1).pending);
    let all = bodies.lock().unwrap();
    let last = all.iter().rev().find(|b| b.contains("feature/coupons")).expect("a digest mentioning the new branch");
    assert!(last.contains("branch changed since the earlier summary: main -> feature/coupons"), "{last}");
    assert_eq!(all.len(), n_before + 1, "only the session whose branch changed is summarized again");
}
