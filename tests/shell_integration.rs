//! Runs real shells under a real `nt server` with shell integration and checks the live status that
//! comes out of the OSC 133 hooks, and that the user's own prompt hooks keep working.

#[allow(dead_code)]
#[path = "../src/proto.rs"]
mod proto;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use proto::{SessionInfo, C2S, PROTO_VERSION, S2C};

struct Daemon {
    child: Child,
    dir: PathBuf,
}

impl Daemon {
    fn start(dir: &Path, shell: &str, extra_env: &[(&str, &Path)]) -> Daemon {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_nt"));
        cmd.args(["server", "--foreground", "--port", "0"])
            .env("XDG_RUNTIME_DIR", dir.join("run"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("HOME", dir)
            .env("SHELL", shell)
            .env_remove("ZDOTDIR")
            .stdin(Stdio::null());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        std::fs::create_dir_all(dir.join("run")).unwrap();
        let child = cmd.spawn().unwrap();
        let d = Daemon { child, dir: dir.to_path_buf() };
        let start = Instant::now();
        while UnixStream::connect(d.sock()).is_err() {
            assert!(start.elapsed() < Duration::from_secs(10), "daemon did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        d
    }

    fn sock(&self) -> PathBuf {
        self.dir.join("run/nt/ctl.sock")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Conn {
    s: UnixStream,
    seq: u64,
}

impl Conn {
    fn open(d: &Daemon) -> Conn {
        let s = UnixStream::connect(d.sock()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let mut c = Conn { s, seq: 0 };
        c.send(&C2S::Hello { proto: PROTO_VERSION, last_input_seq_acked: 0 });
        assert!(matches!(c.recv(), S2C::Welcome { .. }));
        c
    }

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

    /// Wait for a `Sessions` push whose first session satisfies `pred`; returns it and all raw output seen.
    fn wait_status(&mut self, what: &str, pred: impl Fn(&SessionInfo) -> bool, raw: &mut Vec<u8>) -> SessionInfo {
        let start = Instant::now();
        loop {
            assert!(start.elapsed() < Duration::from_secs(20), "timed out waiting for {what}; raw output: {:?}", String::from_utf8_lossy(raw));
            match self.recv() {
                S2C::Sessions(v) => {
                    if let Some(i) = v.into_iter().next().filter(|i| pred(i)) {
                        return i;
                    }
                }
                S2C::Output { data, .. } | S2C::Redraw { data, .. } => raw.extend(data),
                _ => {}
            }
        }
    }
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nt-shell-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn which(bin: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|p| p.join(bin)).find(|p| p.is_file()).map(|p| p.display().to_string())
}

/// Common scenario: `true`, `false`, `sleep 2`, with the user's own prompt hook counting prompts in `$HOME/pc_ran`.
fn scenario(name: &str, shell: &str, setup: impl Fn(&Path), extra_env: &[(&str, &str)]) {
    let dir = tmp(name);
    setup(&dir);
    let envs: Vec<(&str, PathBuf)> = extra_env.iter().map(|(k, v)| (*k, dir.join(v))).collect();
    let envs: Vec<(&str, &Path)> = envs.iter().map(|(k, v)| (*k, v.as_path())).collect();
    let d = Daemon::start(&dir, shell, &envs);
    let mut c = Conn::open(&d);
    c.send(&C2S::Attach { session: None, cols: 100, rows: 30, term: "xterm".into(), resume_from: None , cwd: None });
    let mut raw = Vec::new();

    // Wait for the first prompt: the hooks have then reported the cwd.
    c.wait_status("idle status", |i| i.status.starts_with("idle"), &mut raw);

    c.input("true\n");
    let i = c.wait_status("true", |i| i.status.starts_with("✓ true"), &mut raw);
    assert!(i.status.contains("{ago}") && i.status_since > 0, "{i:?}");

    c.input("false\n");
    let i = c.wait_status("false", |i| i.status.starts_with("✗ false (1)"), &mut raw);
    assert!(i.status.contains("{ago}"), "{i:?}");

    c.input("sleep 2\n");
    let i = c.wait_status("running sleep", |i| i.status.starts_with("⏵ sleep 2"), &mut raw);
    assert!(i.status.contains("{dur}"), "{i:?}");
    let t0 = Instant::now();
    // The user's prompt hook must not have turned into a phantom command, and the OSC sequences reach clients unchanged.
    let i = c.wait_status("sleep done", |i| i.status.starts_with("✓ sleep 2"), &mut raw);
    assert!(t0.elapsed() >= Duration::from_millis(500), "finished too early: {i:?}");
    let text = String::from_utf8_lossy(&raw).into_owned();
    assert!(text.contains("\x1b]133;A"), "no OSC 133;A passed through: {text:?}");
    assert!(text.contains("\x1b]133;D;1"), "no D;1 for `false`: {text:?}");
    assert!(text.contains("\x1b]7;file://"), "no OSC 7: {text:?}");

    // A second false with a ';' and unusual characters in the command line.
    c.input("echo 'a;b' \"é\"; false\n");
    let i = c.wait_status("odd command", |i| i.status.starts_with("✗ "), &mut raw);
    assert!(i.status.contains("echo 'a;b' \"é\"; false") || i.status.contains("false"), "{i:?}");

    // The user's own PROMPT_COMMAND / precmd ran once per prompt (at least the 5 prompts so far).
    let ran = std::fs::read_to_string(dir.join("pc_ran")).unwrap_or_default();
    assert!(ran.lines().count() >= 5, "user prompt hook ran {} times", ran.lines().count());
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bash() {
    let Some(bash) = which("bash") else { return };
    scenario(
        "bash",
        &bash,
        |h| {
            // Login shell: only .bash_profile is read, and it chains to .bashrc like most real ones.
            std::fs::write(h.join(".bash_profile"), ". ~/.bashrc\n").unwrap();
            std::fs::write(
                h.join(".bashrc"),
                "PS1='$ '\nPROMPT_COMMAND='echo x >> \"$HOME/pc_ran\"; ls /dev/null > /dev/null; true'\ntrap 'echo d >> \"$HOME/dbg\"' DEBUG\n",
            )
            .unwrap();
        },
        &[],
    );
}

#[test]
fn bash_keeps_user_debug_trap_and_array_prompt_command() {
    let Some(bash) = which("bash") else { return };
    let dir = tmp("bash2");
    std::fs::write(dir.join(".bash_profile"), "PS1='$ '\nPROMPT_COMMAND=(\"echo x >> $HOME/pc_ran\")\ntrap 'echo d >> \"$HOME/dbg\"' DEBUG\n").unwrap();
    let d = Daemon::start(&dir, &bash, &[]);
    let mut c = Conn::open(&d);
    c.send(&C2S::Attach { session: None, cols: 100, rows: 30, term: "xterm".into(), resume_from: None , cwd: None });
    let mut raw = Vec::new();
    c.wait_status("idle", |i| i.status.starts_with("idle"), &mut raw);
    c.input("true\n");
    c.wait_status("true", |i| i.status.starts_with("✓ true"), &mut raw);
    assert!(std::fs::read_to_string(dir.join("dbg")).unwrap_or_default().lines().count() >= 1, "user DEBUG trap did not run");
    assert!(std::fs::read_to_string(dir.join("pc_ran")).unwrap_or_default().lines().count() >= 2);
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn zsh() {
    let Some(zsh) = which("zsh") else { return };
    scenario(
        "zsh",
        &zsh,
        |h| {
            std::fs::write(h.join(".zshrc"), "PS1='%% '\nprecmd() { echo x >> \"$HOME/pc_ran\"; ls /dev/null >/dev/null }\n").unwrap();
        },
        &[],
    );
}

#[test]
fn fish() {
    let Some(fish) = which("fish") else { return };
    scenario(
        "fish",
        &fish,
        |h| {
            std::fs::create_dir_all(h.join("config/fish")).unwrap();
            std::fs::write(
                h.join("config/fish/config.fish"),
                "function fish_prompt; echo -n '> '; end\nfunction pc --on-event fish_prompt; echo x >> $HOME/pc_ran; end\n",
            )
            .unwrap();
        },
        &[],
    );
}

/// A program that animates the leading glyph of its terminal title and redraws in place, like an AI coding
/// agent: while it runs the status shows the command and the title text (no glyph), after it the title is gone.
#[test]
fn title_in_status() {
    let Some(bash) = which("bash") else { return };
    let dir = tmp("title");
    std::fs::write(dir.join(".bash_profile"), "PS1='$ '\n").unwrap();
    let d = Daemon::start(&dir, &bash, &[]);
    let mut c = Conn::open(&d);
    c.send(&C2S::Attach { session: None, cols: 100, rows: 30, term: "xterm".into(), resume_from: None, cwd: None });
    let mut raw = Vec::new();
    c.wait_status("idle", |i| i.status.starts_with("idle"), &mut raw);
    let script = "for i in $(seq 1 40); do case $((i%4)) in 0) g=✳;; 1) g=⠂;; 2) g=⠐;; *) g=✢;; esac; \
                  printf '\\033]0;%s Fix reconnect bug in proto.rs\\007\\r\\033[2K thinking %d' \"$g\" $i; sleep 0.1; done\n";
    std::fs::write(dir.join("agent.sh"), script).unwrap();
    c.input("bash agent.sh\n");
    let i = c.wait_status("title status", |i| i.status.starts_with("⏵ bash agent.sh") && i.status.contains("Fix reconnect bug"), &mut raw);
    assert!(i.status.contains(" · Fix reconnect bug in proto.rs · {dur}"), "{i:?}");
    assert!(!i.status.contains('✳') && !i.status.contains('⠂'), "{i:?}");
    c.wait_status("done", |i| i.status.starts_with("✓ bash"), &mut raw);
    assert!(String::from_utf8_lossy(&raw).contains("\x1b]0;"), "title not passed through");
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}
