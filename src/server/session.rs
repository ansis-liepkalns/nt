//! One session: a PTY running the user's shell, a vt100 screen model and the output ring buffer.

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Stdio};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
use nix::pty::{openpty, Winsize};
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use tokio::io::unix::AsyncFd;

use super::shell::{self, CommandLog, ShellEv, StatusIn};
use super::transcript::Item;
use crate::proto::{NameSource, SessionId, SessionInfo};

pub const RING_SIZE: usize = 1 << 20;

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The last `cap` bytes of the session output, addressed by absolute stream offset.
pub struct Ring {
    buf: VecDeque<u8>,
    cap: usize,
    end: u64,
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        Ring { buf: VecDeque::new(), cap, end: 0 }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.end += data.len() as u64;
        if data.len() >= self.cap {
            self.buf.clear();
            self.buf.extend(&data[data.len() - self.cap..]);
        } else {
            let over = (self.buf.len() + data.len()).saturating_sub(self.cap);
            self.buf.drain(..over);
            self.buf.extend(data);
        }
    }

    pub fn start(&self) -> u64 {
        self.end - self.buf.len() as u64
    }

    pub fn end(&self) -> u64 {
        self.end
    }

    /// Bytes from absolute offset `off` to the end, if `off` is still buffered.
    pub fn since(&self, off: u64) -> Option<Vec<u8>> {
        if off < self.start() || off > self.end {
            return None;
        }
        Some(self.buf.iter().skip((off - self.start()) as usize).copied().collect())
    }
}

pub struct Session {
    pub id: SessionId,
    pub name: String,
    pub name_source: NameSource,
    /// Repo/dir context at the time the LLM last named this session.
    pub llm_ctx: Option<String>,
    /// Topic from the namer; empty until the first one.
    pub recap: String,
    /// No LLM: the topic is the program's terminal title.
    pub title_topic: bool,
    pub recap_is_title: bool,
    /// Rolling summary from the namer; empty until the first one.
    pub summary: String,
    /// `CommandLog::total` when the summary was made: later commands are the "new activity" of the next run.
    pub summary_seq: u64,
    /// Transcript position (`Transcript::written`) the recap was made at.
    pub recap_mark: u64,
    /// Repo/dir context at the last recap, to notice cwd changes.
    pub recap_ctx: Option<String>,
    /// Something worth re-summarizing happened (command end, cwd change, ssh) since the last namer job.
    pub dirty: bool,
    /// No namer job for this session before this unix time (after a failure).
    pub backoff_until: u64,
    pub tw: super::transcript::Transcript,
    pub created: u64,
    pub last_active: u64,
    /// Last attach or input by a client.
    pub last_used: u64,
    pub ring: Ring,
    pub parser: vt100::Parser,
    pub master: Rc<AsyncFd<OwnedFd>>,
    pub child: Child,
    /// The shell has exited (the session lives on until closed or respawned).
    pub exited: bool,
    /// Kill was requested: when the shell goes away the session ends instead of becoming exited.
    pub closing: bool,
    pub term: String,
    /// Last known cwd of the shell, used to respawn after it exits.
    pub last_cwd: String,
    pub cols: u16,
    pub rows: u16,
    /// Inject the shell hooks (also on respawn).
    pub integration: bool,
    /// Commands seen through the shell hooks.
    pub log: CommandLog,
    /// Last status sent to clients, and when (the push rate limit).
    pub pushed_status: (String, u64),
    pub pushed_at: Option<std::time::Instant>,
    /// Latest resource sample.
    pub cpu_pct: f32,
    pub rss_bytes: u64,
}

fn winsize(cols: u16, rows: u16) -> Winsize {
    Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 }
}

/// Where a new session starts: the requested directory if it exists, else `$HOME`, else the daemon's own directory.
pub fn start_dir(cwd: Option<&str>) -> Option<std::path::PathBuf> {
    let dir = |p: &str| (!p.is_empty() && std::path::Path::new(p).is_absolute() && std::path::Path::new(p).is_dir()).then(|| p.into());
    cwd.and_then(dir).or_else(|| std::env::var("HOME").ok().and_then(|h| dir(&h)))
}

/// A shell on a fresh PTY. Must be called inside the tokio runtime's thread context for the later `AsyncFd`.
fn spawn_shell(id: SessionId, cols: u16, rows: u16, term: &str, cwd: Option<&str>, integration: bool) -> Result<(OwnedFd, Child)> {
    let pty = openpty(Some(&winsize(cols, rows)), None)?;
    // Non-blocking for AsyncFd; close-on-exec so the shell never holds the master
    // (otherwise we would not see EIO when it exits).
    fcntl(&pty.master, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
    fcntl(&pty.master, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let child = {
        let hooks = if integration {
            crate::config::runtime_dir().and_then(|d| shell::install(&d)).map_err(|e| eprintln!("nt: shell integration unavailable: {e:#}")).ok()
        } else {
            None
        };
        let mut cmd = shell::shell_command(&shell, hooks.as_ref());
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
        cmd.env("TERM", term)
            .env("NT_SESSION", id.to_string())
            .stdin(Stdio::from(pty.slave.try_clone()?))
            .stdout(Stdio::from(pty.slave.try_clone()?))
            .stderr(Stdio::from(pty.slave));
        // SAFETY: only async-signal-safe libc calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                nix::libc::setsid();
                nix::libc::ioctl(0, nix::libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        cmd.spawn()?
        // `cmd` is dropped here, closing our copies of the slave side.
    };

    Ok((pty.master, child))
}

impl Session {
    /// Must be called inside the tokio runtime (registers the master fd with the reactor).
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        id: SessionId,
        name: String,
        cols: u16,
        rows: u16,
        term: &str,
        scrollback: usize,
        tdir: Option<&std::path::Path>,
        integration: bool,
        cwd: Option<&str>,
    ) -> Result<Session> {
        let dir = start_dir(cwd);
        let (master, child) = spawn_shell(id, cols, rows, term, dir.as_deref().and_then(|d| d.to_str()), integration)?;
        let now = now_unix();
        let s = Session {
            id,
            name,
            name_source: NameSource::Initial,
            llm_ctx: None,
            recap: String::new(),
            title_topic: false,
            recap_is_title: false,
            summary: String::new(),
            summary_seq: 0,
            recap_mark: 0,
            recap_ctx: None,
            backoff_until: 0,
            dirty: false,
            tw: super::transcript::Transcript::new(tdir, now, id),
            created: now,
            last_active: now,
            last_used: now,
            ring: Ring::new(RING_SIZE),
            parser: vt100::Parser::new(rows, cols, scrollback),
            master: Rc::new(AsyncFd::new(master)?),
            child,
            exited: false,
            closing: false,
            term: term.to_string(),
            last_cwd: String::new(),
            cols,
            rows,
            integration,
            log: CommandLog::default(),
            pushed_status: (String::new(), 0),
            pushed_at: None,
            cpu_pct: 0.0,
            rss_bytes: 0,
        };
        s.save_meta();
        Ok(s)
    }

    /// Record shell output: ring buffer + screen model + transcript.
    pub fn feed(&mut self, data: &[u8]) {
        self.ring.push(data);
        self.parser.process(data);
        self.tw.feed(data);
        let now = now_unix();
        let noise = self.tw.take_noise();
        let items = self.tw.take_items();
        if noise > 0 && !items.iter().any(|i| matches!(i, Item::Ev(ShellEv::Done(_) | ShellEv::Prompt))) {
            self.log.noise(noise);
        }
        for item in items {
            match item {
                Item::Line(l) => self.log.line(&l),
                Item::Ev(ShellEv::Title(t)) => {
                    let home = std::env::var("HOME").unwrap_or_default();
                    let shell_fg = (!self.log.integrated).then(|| self.fg_is_shell());
                    if self.log.note_title(&t, shell_fg, &home, now) {
                        self.dirty = true;
                    }
                    if self.title_topic && (self.recap.is_empty() || self.recap_is_title) && !self.log.title.is_empty() {
                        if let Some(r) = super::namer::sanitize_recap(&self.log.title) {
                            self.recap = r;
                            self.recap_is_title = true;
                        }
                    }
                }
                Item::Ev(e) => {
                    match &e {
                        ShellEv::Done(_) => self.dirty = true,
                        ShellEv::Prompt if self.log.is_running() => self.dirty = true,
                        ShellEv::Cwd(p) if *p != self.log.cwd => self.dirty = true,
                        ShellEv::CmdLine(c) if super::namer::is_ssh(c) => self.dirty = true,
                        _ => {}
                    }
                    self.log.apply(e, now)
                }
            }
        }
        if self.tw.alt_entered() {
            let (fg, cwd) = self.fg_and_cwd();
            self.tw.marker(&format!("{fg} in {cwd}"));
        }
        self.last_active = now_unix();
    }

    /// The shell exited: keep everything, remember where it was.
    pub fn mark_exited(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.exited = true;
        self.tw.marker("shell exited");
    }

    /// Update the remembered cwd from /proc (a no-op once the shell is gone).
    pub fn note_cwd(&mut self) {
        if !self.exited
            && let Ok(p) = std::fs::read_link(format!("/proc/{}/cwd", self.child.id()))
        {
            self.last_cwd = p.display().to_string();
        }
    }

    /// Start a new shell in the last cwd. Same id, screen, ring and transcript.
    pub fn respawn(&mut self) -> Result<()> {
        let cwd = Some(self.last_cwd.as_str()).filter(|d| !d.is_empty() && std::path::Path::new(d).is_dir());
        let (master, child) = spawn_shell(self.id, self.cols, self.rows, &self.term, cwd, self.integration)?;
        self.master = Rc::new(AsyncFd::new(master)?);
        self.child = child;
        self.exited = false;
        self.closing = false;
        // A running command died with the old shell.
        self.log = CommandLog::default();
        self.tw.marker(&format!("shell restarted in {}", self.last_cwd));
        self.last_active = now_unix();
        Ok(())
    }

    pub fn save_meta(&self) {
        self.tw.write_meta(&self.name, self.name_source, &self.recap, &self.summary);
    }

    fn fg_and_cwd(&self) -> (String, String) {
        if self.exited {
            return (String::new(), self.last_cwd.clone());
        }
        let cwd = std::fs::read_link(format!("/proc/{}/cwd", self.child.id()))
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let fg = nix::unistd::tcgetpgrp(self.master.get_ref())
            .ok()
            .and_then(|pg| std::fs::read_to_string(format!("/proc/{pg}/comm")).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        (fg, cwd)
    }

    /// The shell itself is the terminal's foreground process group.
    fn fg_is_shell(&self) -> bool {
        !self.exited && nix::unistd::tcgetpgrp(self.master.get_ref()).is_ok_and(|pg| pg.as_raw() as u32 == self.child.id())
    }

    /// The status line and the time its `{dur}` / `{ago}` count from.
    pub fn status(&self) -> (String, u64) {
        let alt = self.tw.in_alt();
        // Cheap path (this runs several times a second): with working hooks /proc is needed only in an alt-screen app.
        let cheap = self.log.integrated && !alt && !self.log.cwd.is_empty() && !self.exited;
        let (fg, cwd) = if cheap { (String::new(), String::new()) } else { self.fg_and_cwd() };
        let fg_is_shell = self.fg_is_shell();
        let home = std::env::var("HOME").unwrap_or_default();
        let partial = if self.log.is_running() && !alt { self.tw.partial() } else { String::new() };
        self.log.status(&StatusIn { alt, fg_cmd: &fg, fg_is_shell, cwd: &cwd, home: &home, partial: &partial, exited: self.exited })
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 || (cols, rows) == (self.cols, self.rows) {
            return;
        }
        let ws = winsize(cols, rows);
        // SAFETY: valid fd and a pointer to a live Winsize.
        unsafe { nix::libc::ioctl(self.master.as_raw_fd(), nix::libc::TIOCSWINSZ, &ws) };
        self.parser.screen_mut().set_size(rows, cols);
        self.cols = cols;
        self.rows = rows;
    }

    pub fn redraw(&self) -> Vec<u8> {
        crate::proto::repaint(self.parser.screen())
    }

    pub fn signal(&self, sig: Signal) {
        let _ = killpg(Pid::from_raw(self.child.id() as i32), sig);
    }

    pub fn info(&self, clients: u32) -> SessionInfo {
        let (fg_cmd, cwd) = self.fg_and_cwd();
        let (status, status_since) = self.status();
        SessionInfo {
            id: self.id,
            name: self.name.clone(),
            name_source: self.name_source,
            created_unix: self.created,
            last_active_unix: self.last_active,
            last_used_unix: self.last_used,
            clients,
            cols: self.cols,
            rows: self.rows,
            cwd,
            fg_cmd,
            recap: self.recap.clone(),
            summary: self.summary.clone(),
            exited: self.exited,
            status,
            status_since,
            cpu_pct: self.cpu_pct,
            rss_bytes: self.rss_bytes,
            pending: false,
            group: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_offsets() {
        let mut r = Ring::new(8);
        r.push(b"abcd");
        assert_eq!(r.since(2).unwrap(), b"cd");
        assert_eq!(r.since(4).unwrap(), b"");
        assert!(r.since(5).is_none());
        r.push(b"efghij"); // total 10, keeps last 8: "cdefghij"
        assert_eq!((r.start(), r.end()), (2, 10));
        assert!(r.since(1).is_none());
        assert_eq!(r.since(8).unwrap(), b"ij");
        r.push(b"0123456789abc"); // bigger than cap
        assert_eq!(r.since(r.start()).unwrap(), b"56789abc");
        assert_eq!(r.end(), 23);
    }
}
