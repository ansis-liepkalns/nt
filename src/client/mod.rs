//! Terminal client: raw passthrough with a prefix key, the switcher overlay, and a local vt100 mirror
//! so the screen can be repainted after an overlay closes without asking the server.

mod help;
pub mod remote;
pub mod switcher;
mod usage;

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use tokio::signal::unix::{signal, SignalKind};

use crate::config::Config;
use crate::proto::{self, repaint, FrameWriter, SessionId, SessionInfo, C2S, S2C};
use remote::{open_link, Connector, Inbox, Target};
use switcher::{fit, parse_keys, Action, Key, LineEditor, Switcher};

/// Shown on the bottom row while the attached session's shell is gone.
const EXIT_BAR: &str = "shell exited · RET new shell · C-k close";
const PING_EVERY: Duration = Duration::from_secs(5);
const DEAD_AFTER: Duration = Duration::from_secs(15);

pub struct Opts {
    /// `-s`: open the session list instead of starting a new session.
    pub switcher: bool,
    /// Remote destination (`ssh` syntax); `None` = the local daemon.
    pub dest: Option<String>,
    /// Extra arguments for ssh (everything after `--`).
    pub ssh_args: Vec<String>,
}

enum Mode {
    Pass,
    Switcher(Box<Switcher>),
    /// `C-] r`: one-line rename prompt on the bottom row.
    Rename(LineEditor, SessionId),
    /// `C-] k`: `close session? y/n` on the bottom row.
    ConfirmClose(SessionId),
}

struct Client {
    /// `None` while disconnected.
    wr: Option<FrameWriter>,
    /// Remote destination, for status messages; `None` for the local daemon.
    remote: Option<String>,
    /// Input not yet acknowledged by the server, resent after a reconnect.
    inq: VecDeque<(u64, Vec<u8>)>,
    /// Absolute offset in the current session's output stream just past what we have received.
    out_off: u64,
    last_rx: Instant,
    last_ping: Instant,
    prefix: u8,
    seq: u64,
    cols: u16,
    rows: u16,
    term: String,
    sessions: Vec<SessionInfo>,
    cur: Option<SessionId>,
    last: Option<SessionId>,
    mirror: vt100::Parser,
    mode: Mode,
    prefix_pending: bool,
    status_shown: bool,
    /// The server has been told the switcher is open.
    watching: bool,
    /// Set when the client should exit; holds the message to print afterwards (may be empty).
    quit: Option<String>,
    /// The real terminal is on its alternate screen, as far as we have told it.
    naming_error: String,
    naming_paused: String,
    real_alt: bool,
    /// The real terminal's MAIN screen no longer equals the mirror's primary screen (an overlay or status line was
    /// painted on it, or it still holds another session). Entering the alternate screen would save that junk and the
    /// application's `?1049l` would bring it back, so it is cleared first and repainted from the mirror on alt -> main.
    main_stale: bool,
    /// Test hook: collect the terminal output here instead of writing to stdout.
    capture: Option<Vec<u8>>,
    /// Show the one-time key tip (config `tips`).
    tips: bool,
    /// `C-] ?` help box is showing on top of the current mode.
    help: bool,
    /// When the tip on the bottom row is taken down again.
    tip_until: Option<Instant>,
}

/// Restores the terminal on drop (normal exit, error or panic unwinding).
struct TermGuard;

impl TermGuard {
    fn new() -> Result<Self> {
        enable_raw_mode()?;
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            reset_terminal();
            default_hook(info);
        }));
        Ok(TermGuard)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        reset_terminal();
    }
}

fn reset_terminal() {
    let _ = disable_raw_mode();
    let rows = size().ok().map(|(_, r)| r);
    let _ = write_out(&reset_sequence(rows));
}

/// What leaves the terminal usable after nt: attributes, cursor, main screen, input modes (undo whatever the session
/// enabled), then a fresh line below everything on the screen. The cursor is first moved to the bottom row, because after
/// a session repaint it sits wherever the session's cursor was, and the exit message would overwrite the screen there.
fn reset_sequence(rows: Option<u16>) -> Vec<u8> {
    let mut v = b"\x1b[0m\x1b[?25h\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b[?1l\x1b>".to_vec();
    if let Some(r) = rows {
        v.extend_from_slice(format!("\x1b[{r};1H").as_bytes());
    }
    v.extend_from_slice(b"\r\n");
    v
}

fn write_out(data: &[u8]) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(data)?;
    out.flush()
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub fn run(cfg: &Config, opts: Opts) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let msg = rt.block_on(session(cfg, opts))?;
    if !msg.is_empty() {
        eprintln!("nt: {msg}");
    }
    Ok(())
}

fn spawn_stdin(paused: Arc<AtomicBool>) -> tokio::sync::mpsc::UnboundedReceiver<Vec<u8>> {
    // Raw stdin on a plain thread: a blocking read can't be cancelled, and the process exits anyway.
    // It polls so it can stand aside (`paused`) while ssh needs the terminal for a password prompt.
    let (itx, irx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut stdin = std::io::stdin();
        loop {
            if paused.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            let mut pfd = nix::libc::pollfd { fd: 0, events: nix::libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd.
            let r = unsafe { nix::libc::poll(&mut pfd, 1, 100) };
            if r <= 0 || paused.load(Ordering::SeqCst) {
                continue;
            }
            match stdin.read(&mut buf) {
                Ok(n) if n > 0 && itx.send(buf[..n].to_vec()).is_ok() => {}
                _ => break,
            }
        }
    });
    irx
}

/// Resolve the target and (for remote) run the ssh bootstrap. Must be called before raw mode.
pub fn prepare(opts_dest: Option<&str>, ssh_args: &[String]) -> Result<(Target, Option<remote::Creds>)> {
    match opts_dest {
        None => Ok((Target::Local, None)),
        Some(d) => {
            let t = Target::remote(d, ssh_args.to_vec());
            let creds = remote::ssh_bootstrap(d, ssh_args)?;
            Ok((t, Some(creds)))
        }
    }
}

/// One line for `nt ls`: id, name, status (or the foreground command), topic.
fn ls_entry(s: &SessionInfo, now: u64) -> String {
    let status = if s.exited {
        "exited".to_string()
    } else if !s.status.is_empty() {
        crate::proto::expand_status(&s.status, s.status_since, now)
    } else {
        s.fg_cmd.clone()
    };
    format!("#{:<3} {:<24} {:<32} {}", s.id, s.name, status, s.recap).trim_end().to_string()
}

/// Cut `line` to `width` columns with a trailing `…` (the topic goes first, then the status); no limit = unchanged.
fn fit_line(line: &str, width: Option<usize>) -> String {
    match width {
        Some(w) if w > 1 && line.chars().count() > w => {
            let mut t: String = line.chars().take(w - 1).collect();
            t.truncate(t.trim_end().len());
            t + "…"
        }
        _ => line.to_string(),
    }
}

/// `nt ls [dest]`
pub fn ls(dest: Option<&str>, ssh_args: &[String]) -> Result<()> {
    let (target, creds) = prepare(dest, ssh_args)?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let (sessions, naming_error) = rt.block_on(async {
        let mut link = open_link(&target, creds.as_ref(), 0).await?;
        link.wr.write_msg(&C2S::List).await?;
        let mut err = String::new();
        loop {
            match link.inbox.rx.recv().await {
                Some(S2C::NamerError(e)) => err = e,
                Some(S2C::Sessions(v)) => break Ok::<_, anyhow::Error>((v, err)),
                Some(_) => {}
                None => bail!("server closed the connection"),
            }
        }
    })?;
    if sessions.is_empty() {
        println!("no sessions");
    }
    let now = now_unix();
    // On a terminal every session fits on one line; piped output keeps the full text.
    let width = std::io::IsTerminal::is_terminal(&std::io::stdout()).then(|| size().map(|(c, _)| c as usize).ok()).flatten();
    for s in &sessions {
        println!("{}", fit_line(&ls_entry(s, now), width));
    }
    if !naming_error.is_empty() {
        println!("naming: error: {naming_error}");
    }
    Ok(())
}

async fn session(cfg: &Config, opts: Opts) -> Result<String> {
    let (target, creds) = prepare(opts.dest.as_deref(), &opts.ssh_args)?;
    let mut link = open_link(&target, creds.as_ref(), 0).await?;
    let paused = Arc::new(AtomicBool::new(false));
    let mut connector = Connector::new(target, creds, paused.clone());

    link.wr.write_msg(&C2S::List).await?;
    let sessions = loop {
        match link.inbox.rx.recv().await {
            Some(S2C::Sessions(v)) => break v,
            Some(_) => {}
            None => bail!("server closed the connection"),
        }
    };

    let (cols, rows) = term_size();
    let mut c = Client {
        wr: Some(link.wr),
        remote: opts.dest.clone(),
        inq: VecDeque::new(),
        out_off: 0,
        last_rx: Instant::now(),
        last_ping: Instant::now(),
        prefix: cfg.prefix,
        seq: 0,
        cols,
        rows,
        term: std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
        mirror: vt100::Parser::new(rows, cols, 0),
        sessions,
        cur: None,
        last: None,
        mode: Mode::Pass,
        prefix_pending: false,
        status_shown: false,
        watching: false,
        quit: None,
        naming_error: String::new(),
        naming_paused: String::new(),
        real_alt: false,
        main_stale: false,
        capture: None,
        tips: cfg.tips,
        help: false,
        tip_until: None,
    };
    let inbox = link.inbox;

    // `nt` starts a new session; `nt -s` opens the list (a new session directly when there is none).
    let target = if opts.switcher && !c.sessions.is_empty() {
        let _guard = TermGuard::new()?;
        connector.raw = true;
        c.open_switcher();
        return interactive(c, inbox, connector).await;
    } else {
        None
    };
    let _guard = TermGuard::new()?;
    connector.raw = true;
    c.attach(target).await?;
    interactive(c, inbox, connector).await
}

async fn recv_opt(inbox: &mut Option<Inbox>) -> Option<S2C> {
    match inbox {
        Some(i) => i.rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until_opt(t: Option<Instant>) {
    match t {
        Some(t) => tokio::time::sleep_until(tokio::time::Instant::from_std(t)).await,
        None => std::future::pending().await,
    }
}

async fn interactive(mut c: Client, inbox: Inbox, mut connector: Connector) -> Result<String> {
    let mut irx = spawn_stdin(connector.stdin_paused.clone());
    let mut winch = signal(SignalKind::window_change())?;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut spin = tokio::time::interval(SPIN_EVERY);
    spin.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut inbox = Some(inbox);
    let remote = c.remote.is_some();

    while c.quit.is_none() {
        tokio::select! {
            chunk = irx.recv() => match chunk {
                Some(data) => c.on_input(&data).await?,
                None => c.quit = Some("input closed".into()),
            },
            msg = recv_opt(&mut inbox) => match msg {
                Some(m) => {
                    c.last_rx = Instant::now();
                    c.on_server(m).await?;
                }
                None => inbox = None,
            },
            _ = winch.recv() => c.on_resize().await?,
            _ = sleep_until_opt(c.search_due()) => c.fire_search().await?,
            _ = sleep_until_opt(c.tip_until) => c.tip_expired(),
            // The pending-topic spinner: runs only while the switcher shows a pending session.
            _ = spin.tick(), if c.spinning() => c.spin_frame(),
            _ = tick.tick(), if inbox.is_some() => {
                if c.last_rx.elapsed() > DEAD_AFTER {
                    inbox = None;
                } else if c.last_ping.elapsed() >= PING_EVERY {
                    c.last_ping = Instant::now();
                    c.send(C2S::Ping).await?;
                }
            }
            r = connector.next(), if inbox.is_none() && remote => match r {
                Ok(link) => {
                    c.wr = Some(link.wr);
                    inbox = Some(link.inbox);
                    c.on_reconnect(link.welcome_seq).await?;
                }
                Err(e) if e.is::<proto::VersionMismatch>() => c.quit = Some(format!("{e}")),
                Err(e) => {
                    let msg = format!("reconnecting to {}… (C-] q quit) [{e:#}]", c.remote.as_deref().unwrap_or(""));
                    c.status(&msg);
                }
            },
        }
        if c.wr.is_none() && inbox.is_some() {
            inbox = None; // a failed write counts as a dead connection
        }
        if inbox.is_none() {
            c.wr = None;
            if !remote {
                c.quit.get_or_insert_with(|| "server disconnected".into());
            } else if c.quit.is_none() && !c.status_shown {
                let msg = format!("reconnecting to {}… (C-] q quit)", c.remote.as_deref().unwrap_or(""));
                c.status(&msg);
            }
        }
        c.sync_watch().await?;
        connector.acked = c.inq.front().map_or(c.seq, |(s, _)| s - 1);
    }
    Ok(c.quit.unwrap_or_default())
}

/// Spinner frame time (about 8 fps).
const SPIN_EVERY: Duration = Duration::from_millis(125);

impl Client {
    fn spinning(&self) -> bool {
        matches!(&self.mode, Mode::Switcher(sw) if sw.any_pending())
    }

    fn spin_frame(&mut self) {
        if let Mode::Switcher(sw) = &mut self.mode {
            sw.tick_spinner();
            self.draw_overlay();
        }
    }

    /// Tell the server whether the switcher is open (it samples resource use faster while it is).
    async fn sync_watch(&mut self) -> Result<()> {
        let want = matches!(self.mode, Mode::Switcher(_));
        if want != self.watching && self.wr.is_some() {
            self.watching = want;
            self.send(C2S::Watch(want)).await?;
        }
        Ok(())
    }

    /// Send if connected; a failed or stalled write drops the connection (the main loop notices).
    async fn send(&mut self, msg: C2S) -> Result<()> {
        if let Some(w) = &mut self.wr {
            let r = tokio::time::timeout(Duration::from_secs(10), w.write_msg(&msg)).await;
            if !matches!(r, Ok(Ok(()))) {
                self.wr = None;
            }
        }
        Ok(())
    }

    fn search_due(&self) -> Option<Instant> {
        match &self.mode {
            Mode::Switcher(sw) => sw.search_due(),
            _ => None,
        }
    }

    /// The debounce of the switcher's full-text search expired: send the query.
    async fn fire_search(&mut self) -> Result<()> {
        let q = match &mut self.mode {
            Mode::Switcher(sw) => sw.take_search(),
            _ => None,
        };
        match q {
            Some(query) => self.send(C2S::Search { query }).await,
            None => {
                self.draw_overlay();
                Ok(())
            }
        }
    }

    /// Input typed while disconnected is dropped; input sent just before a drop stays queued until acked.
    async fn send_input(&mut self, data: Vec<u8>) -> Result<()> {
        if data.is_empty() || self.wr.is_none() {
            return Ok(());
        }
        self.seq += 1;
        self.inq.push_back((self.seq, data.clone()));
        self.send(C2S::Input { seq: self.seq, data }).await
    }

    fn ack(&mut self, seq: u64) {
        while self.inq.front().is_some_and(|(s, _)| *s <= seq) {
            self.inq.pop_front();
        }
    }

    /// New connection after `Welcome`: resend unacked input, then resume the session (or just list).
    async fn on_reconnect(&mut self, welcome_seq: u64) -> Result<()> {
        self.watching = false; // a new connection starts unwatched
        self.ack(welcome_seq);
        self.last_rx = Instant::now();
        self.last_ping = Instant::now();
        let pending: Vec<(u64, Vec<u8>)> = self.inq.iter().cloned().collect();
        for (seq, data) in pending {
            self.send(C2S::Input { seq, data }).await?;
        }
        if self.cur.is_some() {
            if !matches!(self.mode, Mode::Pass) {
                self.mode = Mode::Pass;
            }
            self.repaint();
            let msg = C2S::Attach {
                session: self.cur,
                cols: self.cols,
                rows: self.rows,
                term: self.term.clone(),
                resume_from: Some(self.out_off),
                cwd: None,
            };
            self.send(msg).await
        } else {
            self.send(C2S::List).await
        }
    }

    async fn attach(&mut self, session: Option<SessionId>) -> Result<()> {
        let msg = C2S::Attach {
            session,
            cols: self.cols,
            rows: self.rows,
            term: self.term.clone(),
            resume_from: None,
            cwd: if session.is_none() { self.new_session_cwd() } else { None },
        };
        self.send(msg).await
    }

    /// Where a new session should start: next to the current session (like `tmux -c`), else, for a local daemon, in
    /// this process's directory. A remote daemon starts in the remote `$HOME`.
    fn new_session_cwd(&self) -> Option<String> {
        let cur = self.cur.and_then(|id| self.sessions.iter().find(|s| s.id == id)).map(|s| s.cwd.clone()).filter(|c| !c.is_empty());
        cur.or_else(|| {
            if self.remote.is_some() {
                None
            } else {
                std::env::current_dir().ok().and_then(|d| d.to_str().map(String::from))
            }
        })
    }

    // ------------------------------------------------------------ input

    async fn on_input(&mut self, chunk: &[u8]) -> Result<()> {
        if self.help {
            // Any key closes the help; repaint from the mirror, then whatever overlay was under it.
            self.help = false;
            self.repaint();
            self.draw_overlay();
            return Ok(());
        }
        if self.status_shown && matches!(self.mode, Mode::Pass) {
            self.repaint();
        }
        let mut data = chunk;
        while !data.is_empty() && self.quit.is_none() {
            if matches!(self.mode, Mode::Pass) {
                let n = self.pass_bytes(data).await?;
                data = &data[n..];
            } else {
                self.overlay_keys(data).await?;
                break;
            }
        }
        Ok(())
    }

    /// Passthrough: forward bytes, interpreting the prefix key. Returns how many bytes were consumed
    /// (fewer than `data.len()` when a command changed the mode).
    async fn pass_bytes(&mut self, data: &[u8]) -> Result<usize> {
        let mut fwd = Vec::new();
        for (i, &b) in data.iter().enumerate() {
            if self.prefix_pending {
                self.prefix_pending = false;
                self.send_input(std::mem::take(&mut fwd)).await?;
                self.prefix_cmd(b).await?;
                if !matches!(self.mode, Mode::Pass) || self.quit.is_some() {
                    return Ok(i + 1);
                }
            } else if b == self.prefix {
                self.prefix_pending = true;
            } else if self.cur_exited() {
                // Nothing is listening: swallow typing until the shell is respawned or the session closed.
                match exited_key(data, i) {
                    ExitedKey::Respawn => {
                        if let Some(session) = self.cur {
                            self.send(C2S::Respawn { session }).await?;
                        }
                    }
                    ExitedKey::Close => {
                        // The exit itself was the first step: no y/n.
                        if let Some(session) = self.cur {
                            self.send(C2S::Kill { session }).await?;
                        }
                        return Ok(data.len());
                    }
                    ExitedKey::Ignore => {}
                }
            } else {
                fwd.push(b);
            }
        }
        self.send_input(fwd).await?;
        Ok(data.len())
    }

    async fn prefix_cmd(&mut self, b: u8) -> Result<()> {
        if b == self.prefix {
            self.open_switcher();
            return Ok(());
        }
        match b {
            b's' => self.open_switcher(),
            b'?' => self.show_help(),
            b'n' => self.cycle(1).await?,
            b'p' => self.cycle(-1).await?,
            b'l' => {
                if let Some(l) = self.last.filter(|l| Some(*l) != self.cur && self.sessions.iter().any(|s| s.id == *l)) {
                    self.attach(Some(l)).await?;
                }
            }
            b'c' => self.attach(None).await?,
            b'r' => {
                if let Some(s) = self.sessions.iter().find(|s| Some(s.id) == self.cur) {
                    self.mode = Mode::Rename(LineEditor::with_text(&s.name), s.id);
                    self.draw_overlay();
                }
            }
            b'd' => {
                self.send(C2S::Detach).await?;
                self.quit = Some("detached".into());
            }
            b'k' => {
                if let Some(id) = self.cur {
                    if self.cur_exited() {
                        self.send(C2S::Kill { session: id }).await?;
                    } else {
                        self.mode = Mode::ConfirmClose(id);
                        self.draw_overlay();
                    }
                }
            }
            b'q' => self.quit = Some(String::new()),
            b']' => self.send_input(vec![self.prefix]).await?,
            _ => {}
        }
        Ok(())
    }

    /// Next/previous session by id.
    async fn cycle(&mut self, dir: isize) -> Result<()> {
        let n = self.sessions.len() as isize;
        if n < 2 {
            return Ok(());
        }
        let idx = self.sessions.iter().position(|s| Some(s.id) == self.cur).unwrap_or(0) as isize;
        let id = self.sessions[((idx + dir).rem_euclid(n)) as usize].id;
        self.attach(Some(id)).await
    }

    /// Keys while the switcher or the rename prompt is open.
    async fn overlay_keys(&mut self, data: &[u8]) -> Result<()> {
        for key in parse_keys(data) {
            match &mut self.mode {
                Mode::Switcher(sw) => {
                    let action = sw.handle(key);
                    self.apply(action).await?;
                }
                Mode::Rename(ed, sid) => match key {
                    Key::Enter => {
                        let (name, sid) = (ed.text(), *sid);
                        self.close_overlay();
                        self.send(C2S::Rename { session: sid, name }).await?;
                    }
                    Key::Ctrl('g') | Key::Esc => self.close_overlay(),
                    k => {
                        ed.handle(k);
                    }
                },
                Mode::ConfirmClose(sid) => {
                    let sid = *sid;
                    self.close_overlay();
                    if key == Key::Char('y') {
                        self.send(C2S::Kill { session: sid }).await?;
                    }
                }
                Mode::Pass => break,
            }
            if self.quit.is_some() || matches!(self.mode, Mode::Pass) {
                return Ok(());
            }
        }
        self.draw_overlay();
        Ok(())
    }

    async fn apply(&mut self, action: Action) -> Result<()> {
        match action {
            Action::None => {}
            Action::Cancel => {
                if self.cur.is_some() {
                    self.close_overlay();
                } else {
                    self.quit = Some(String::new());
                }
            }
            Action::Attach(id) if Some(id) == self.cur => self.close_overlay(),
            Action::Attach(id) => self.attach(Some(id)).await?,
            Action::New => self.attach(None).await?,
            Action::Rename(session, name) => self.send(C2S::Rename { session, name }).await?,
            Action::Kill(session) => self.send(C2S::Kill { session }).await?,
            Action::Help => self.show_help(),
            Action::Detach => {
                self.send(C2S::Detach).await?;
                self.quit = Some("detached".into());
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ server messages

    async fn on_server(&mut self, msg: S2C) -> Result<()> {
        match msg {
            S2C::Sessions(v) => {
                let was = self.cur_exited();
                self.sessions = v;
                if matches!(self.mode, Mode::Pass) && (was != self.cur_exited() || (self.cur_exited() && !self.status_shown)) {
                    self.repaint();
                }
                if let Mode::Switcher(sw) = &mut self.mode {
                    sw.update(self.sessions.clone(), self.cur);
                    self.draw_overlay();
                }
            }
            S2C::Attached { session } => {
                if self.cur != Some(session) {
                    self.last = self.cur;
                    // The main screen holds the previous session (or the switcher).
                    self.main_stale = true;
                }
                self.cur = Some(session);
                self.mode = Mode::Pass;
                self.prefix_pending = false;
            }
            S2C::Output { offset, data } => {
                self.out_off = offset + data.len() as u64;
                let was_alt = self.mirror.screen().alternate_screen();
                self.mirror.process(&data);
                if matches!(self.mode, Mode::Pass) {
                    self.emit(&data);
                    let now_alt = self.mirror.screen().alternate_screen();
                    self.real_alt = now_alt;
                    if self.main_stale && was_alt != now_alt {
                        // alt -> main: the terminal restored a stale main screen; main -> alt: it saved one.
                        self.repaint();
                    }
                }
            }
            S2C::Redraw { offset, data } => {
                self.out_off = offset;
                self.mirror = vt100::Parser::new(self.rows, self.cols, 0);
                self.mirror.process(&data);
                if matches!(self.mode, Mode::Pass) {
                    self.repaint(); // from the fresh mirror; clears the stale main screen first if needed
                    self.show_tip();
                }
            }
            S2C::SessionEnded { session } => {
                self.sessions.retain(|s| s.id != session);
                if self.last == Some(session) {
                    self.last = None;
                }
                if self.cur == Some(session) {
                    self.cur = None;
                    if self.sessions.is_empty() {
                        self.quit = Some("session ended".into());
                    } else if matches!(self.mode, Mode::Pass) {
                        self.open_switcher();
                    }
                }
            }
            S2C::Error(e) => self.status(&e),
            S2C::Ack { last_input_seq } => self.ack(last_input_seq),
            S2C::SearchResults(hits) => {
                if let Mode::Switcher(sw) = &mut self.mode {
                    sw.set_hits(hits);
                    self.draw_overlay();
                }
            }
            S2C::NamerError(e) => {
                self.naming_error = e;
                if let Mode::Switcher(sw) = &mut self.mode {
        sw.set_naming_error(&self.naming_error);
                    self.draw_overlay();
                }
            }
            S2C::NamerPaused(why) => {
                self.naming_paused = why;
                if let Mode::Switcher(sw) = &mut self.mode {
                    sw.set_naming_paused(&self.naming_paused);
                    self.draw_overlay();
                }
            }
            S2C::Welcome { .. } | S2C::Pong | S2C::Registered { .. } => {}
        }
        Ok(())
    }

    async fn on_resize(&mut self) -> Result<()> {
        let (cols, rows) = term_size();
        (self.cols, self.rows) = (cols, rows);
        self.mirror.screen_mut().set_size(rows, cols);
        self.send(C2S::Resize { cols, rows }).await?;
        if !matches!(self.mode, Mode::Pass) || self.help {
            self.emit(b"\x1b[2J");
            self.draw_overlay();
        }
        Ok(())
    }

    // ------------------------------------------------------------ drawing

    fn open_switcher(&mut self) {
        let mut sw = Switcher::new(self.sessions.clone(), self.cur);
        sw.set_prefix(self.prefix);
        sw.set_naming_error(&self.naming_error);
        sw.set_naming_paused(&self.naming_paused);
        self.mode = Mode::Switcher(Box::new(sw));
        self.draw_overlay();
    }

    fn close_overlay(&mut self) {
        self.mode = Mode::Pass;
        self.repaint();
    }

    /// Repaint the whole screen from the local mirror.
    fn repaint(&mut self) {
        self.status_shown = false;
        self.tip_until = None;
        let screen = self.mirror.screen();
        let mut out = Vec::new();
        if screen.alternate_screen() {
            if self.main_stale {
                // Never let the terminal save a stale main screen: clear it before entering the alternate one.
                if self.real_alt {
                    out.extend_from_slice(b"\x1b[?1049l");
                }
                out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
            }
        } else {
            self.main_stale = false; // the repaint below redraws the main screen completely
        }
        out.extend_from_slice(&repaint(screen));
        self.real_alt = screen.alternate_screen();
        self.emit(&out);
        if self.cur_exited() && matches!(self.mode, Mode::Pass) {
            self.status(EXIT_BAR);
        }
    }

    /// The attached session's shell has exited.
    fn cur_exited(&self) -> bool {
        self.sessions.iter().any(|s| Some(s.id) == self.cur && s.exited)
    }

    fn show_help(&mut self) {
        self.help = true;
        self.draw_overlay();
    }

    fn draw_overlay(&mut self) {
        let mut out = match &mut self.mode {
            Mode::Switcher(sw) => sw.render(self.cols, self.rows, now_unix()),
            Mode::Rename(ed, _) => {
                let label = "rename (empty = auto): ";
                let line = fit(&format!("{label}{}", ed.text()), self.cols as usize);
                let col = (label.len() + ed.cursor() + 1).min(self.cols as usize);
                format!("\x1b[{};1H\x1b[7m{line}\x1b[0m\x1b[{};{col}H", self.rows, self.rows)
            }
            Mode::ConfirmClose(_) => {
                let line = fit("close session? y/n", self.cols as usize);
                format!("\x1b[{};1H\x1b[7m{line}\x1b[0m\x1b[{};1H", self.rows, self.rows)
            }
            Mode::Pass => String::new(),
        };
        if self.help {
            out.push_str(&help::render(self.cols, self.rows, self.prefix, usage::ascii_mode()));
        }
        if !out.is_empty() {
            self.paint_over(out.as_bytes());
        }
    }

    /// Write terminal output.
    fn emit(&mut self, data: &[u8]) {
        match &mut self.capture {
            Some(c) => c.extend_from_slice(data),
            None => {
                let _ = write_out(data);
            }
        }
    }

    /// Paint something that is not part of the session (overlay, status line) on the current screen.
    fn paint_over(&mut self, data: &[u8]) {
        if !self.real_alt {
            self.main_stale = true;
        }
        self.emit(data);
    }

    /// The key tip, once per client run: dim on the bottom row for 3 seconds (or until the next key).
    fn show_tip(&mut self) {
        if !self.tips || self.cur_exited() || !matches!(self.mode, Mode::Pass) {
            return;
        }
        self.tips = false;
        let p = help::prefix_name(self.prefix);
        let full = format!("nt: {p} {p} sessions · {p} ? keys · {p} d detach");
        let short = format!("nt: {p} ? keys · {p} d detach");
        let line = fit(if full.chars().count() <= self.cols as usize { &full } else { &short }, self.cols as usize);
        self.paint_over(format!("\x1b7\x1b[{};1H\x1b[2m{line}\x1b[0m\x1b8", self.rows).as_bytes());
        self.status_shown = true;
        self.tip_until = Some(Instant::now() + Duration::from_secs(3));
    }

    /// The tip's 3 seconds are over: repaint the row from the mirror.
    fn tip_expired(&mut self) {
        self.tip_until = None;
        if self.status_shown && matches!(self.mode, Mode::Pass) {
            self.repaint();
        }
    }

    /// Temporary message on the bottom row; the next keypress repaints from the mirror.
    fn status(&mut self, msg: &str) {
        let line = if msg == EXIT_BAR { fit(msg, self.cols as usize) } else { fit(&format!("nt: {msg}"), self.cols as usize) };
        self.paint_over(format!("\x1b7\x1b[{};1H\x1b[7m{line}\x1b[0m\x1b8", self.rows).as_bytes());
        self.status_shown = true;
        self.tip_until = None;
    }
}

/// What a key typed on the exited-session screen does.
#[derive(Debug, PartialEq, Eq)]
enum ExitedKey {
    /// RET: start a new shell here.
    Respawn,
    /// C-k or M-k (ESC k): close the session.
    Close,
    Ignore,
}

/// The meaning of the byte `data[i]` on the exited-session screen (M-k arrives as ESC followed by `k`).
fn exited_key(data: &[u8], i: usize) -> ExitedKey {
    match data[i] {
        b'\r' | b'\n' => ExitedKey::Respawn,
        0x0b => ExitedKey::Close,
        0x1b if data.get(i + 1) == Some(&b'k') => ExitedKey::Close,
        _ => ExitedKey::Ignore,
    }
}

/// `nt bootstrap` (run through ssh): make sure the daemon runs, register a client, print the credentials.
pub fn bootstrap() -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let line = rt.block_on(async {
        let mut link = open_link(&Target::Local, None, 0).await?;
        link.wr.write_msg(&C2S::Register).await?;
        loop {
            match link.inbox.rx.recv().await {
                Some(S2C::Registered { port, client_id, key }) => {
                    break Ok::<_, anyhow::Error>(remote::bootstrap_line(port, &client_id, &key))
                }
                Some(S2C::Error(e)) => bail!("{e}"),
                Some(_) => {}
                None => bail!("server closed the connection"),
            }
        }
    })?;
    println!("{line}");
    Ok(())
}

/// Terminal size, falling back to 80x24 when the terminal reports nothing usable (0x0 on some serial consoles,
/// containers and bare ptys), so the session still starts.
fn term_size() -> (u16, u16) {
    match size() {
        Ok((c, r)) if c > 0 && r > 0 => (c, r),
        _ => (80, 24),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the real terminal modelled by a second vt100 parser
    fn test_client() -> Client {
        Client {
            wr: None,
            remote: None,
            inq: VecDeque::new(),
            out_off: 0,
            last_rx: Instant::now(),
            last_ping: Instant::now(),
            prefix: 0x1d,
            seq: 0,
            cols: 40,
            rows: 10,
            term: "xterm".into(),
            sessions: Vec::new(),
            cur: None,
            last: None,
            mirror: vt100::Parser::new(10, 40, 0),
            mode: Mode::Pass,
            prefix_pending: false,
            status_shown: false,
            watching: false,
            quit: None,
            naming_error: String::new(),
            naming_paused: String::new(),
        real_alt: false,
            main_stale: false,
            capture: Some(Vec::new()),
            tips: false,
            help: false,
            tip_until: None,
        }
    }

    struct Harness {
        c: Client,
        real: vt100::Parser,
        rt: tokio::runtime::Runtime,
    }

    impl Harness {
        fn new() -> Self {
            Harness {
                c: test_client(),
                real: vt100::Parser::new(10, 40, 0),
                rt: tokio::runtime::Builder::new_current_thread().build().unwrap(),
            }
        }
        fn flush(&mut self) {
            let out = self.c.capture.as_mut().map(std::mem::take).unwrap_or_default();
            self.real.process(&out);
        }
        fn server(&mut self, m: S2C) {
            self.rt.block_on(self.c.on_server(m)).unwrap();
            self.flush();
        }
        fn out(&mut self, data: &[u8]) {
            let offset = self.c.out_off;
            self.server(S2C::Output { offset, data: data.to_vec() });
        }
        fn attach(&mut self, id: SessionId, redraw_of: &[u8]) {
            self.server(S2C::Attached { session: id });
            let mut p = vt100::Parser::new(10, 40, 0);
            p.process(redraw_of);
            self.server(S2C::Redraw { offset: 0, data: repaint(p.screen()) });
        }
        fn open(&mut self) {
            self.c.open_switcher();
            self.flush();
        }
        fn close(&mut self) {
            self.c.close_overlay();
            self.flush();
        }
        fn real_text(&self) -> String {
            self.real.screen().contents()
        }
        /// The real terminal must show exactly what the mirror shows, and no switcher.
        fn assert_synced(&self, what: &str) {
            assert_eq!(self.real.screen().alternate_screen(), self.c.mirror.screen().alternate_screen(), "{what}: alt flag");
            assert_eq!(self.real_text(), self.c.mirror.screen().contents(), "{what}");
            assert!(!self.real_text().contains("switch") && !self.real_text().contains("filter"), "{what}: overlay left behind");
        }
    }

    #[test]
    fn detach_leaves_the_terminal_clean() {
        let mut h = Harness::new();
        // A full-screen application with a hidden cursor and input modes on, the cursor near the top.
        h.attach(1, b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[?1h\x1b[H\x1b[2Jeditor line 1\r\nline 2");
        h.rt.block_on(h.c.overlay_keys(b"")).ok();
        assert!(h.real.screen().alternate_screen() && h.real.screen().hide_cursor());
        // And from the main screen: output, cursor in the middle of the page.
        let mut h2 = Harness::new();
        h2.attach(1, b"$ ls\r\nfile-a  file-b\r\n$ make\r\nbuilding...\x1b[3;1H");
        for hh in [&mut h, &mut h2] {
            let was_alt = hh.real.screen().alternate_screen();
            let out = reset_sequence(Some(10));
            hh.real.process(&out);
            hh.real.process(b"nt: detached\r\n");
            let s = hh.real.screen();
            assert!(!s.alternate_screen() && !s.hide_cursor(), "main screen, cursor visible");
            assert_eq!(s.mouse_protocol_mode(), vt100::MouseProtocolMode::None);
            assert!(!s.bracketed_paste() && !s.application_cursor());
            // the message is on its own line at the bottom; nothing above was overwritten
            let text = s.contents();
            assert!(text.lines().last().unwrap().starts_with("nt: detached"), "{text}");
            if !was_alt {
                // the screen only scrolled up by the two new lines; the newest output is intact above the message
                assert!(text.contains("$ make\nbuilding...") && text.lines().rev().nth(1).is_some_and(|l| l.is_empty()), "{text}");
            }
            assert_eq!(s.cursor_position().1, 0, "cursor at column 0");
        }
    }

    #[test]
    fn new_session_starts_next_to_the_current_one() {
        let mut h = Harness::new();
        let info = |id: SessionId, cwd: &str| SessionInfo {
            id,
            name: "n".into(),
            name_source: crate::proto::NameSource::Initial,
            created_unix: 0,
            last_active_unix: 0,
            last_used_unix: 0,
            clients: 0,
            cols: 80,
            rows: 24,
            cwd: cwd.into(),
            fg_cmd: String::new(),
            recap: String::new(),
            summary: String::new(),
            exited: false,
            status: String::new(),
            status_since: 0,
            cpu_pct: 0.0,
            rss_bytes: 0,
            pending: false,
        };
        h.c.sessions = vec![info(1, "/srv/app"), info(2, "/var/log")];
        h.c.cur = Some(2);
        assert_eq!(h.c.new_session_cwd().as_deref(), Some("/var/log"));
        // no current session: a local client uses its own directory, a remote one lets the server pick its $HOME
        h.c.cur = None;
        assert_eq!(h.c.new_session_cwd(), std::env::current_dir().unwrap().to_str().map(String::from));
        h.c.remote = Some("box".into());
        assert_eq!(h.c.new_session_cwd(), None);
    }

    #[test]
    fn tip_shows_once_then_repaints() {
        let mut h = Harness::new();
        h.c.tips = true;
        h.server(S2C::Attached { session: 1 });
        let mut p = vt100::Parser::new(10, 40, 0);
        p.process(b"user@host:~$ ");
        h.server(S2C::Redraw { offset: 0, data: repaint(p.screen()) });
        let last = |h: &Harness| h.real.screen().contents().lines().last().unwrap_or("").to_string();
        assert!(last(&h).contains("C-] d detach"), "{}", h.real_text());
        assert!(h.c.tip_until.is_some());
        h.c.tip_expired();
        h.flush();
        h.assert_synced("after the tip");
        // once per client run
        h.server(S2C::Redraw { offset: 0, data: repaint(p.screen()) });
        assert!(h.c.tip_until.is_none());
        h.assert_synced("second redraw");
    }

    #[test]
    fn tip_disabled_by_config_and_detach_from_switcher() {
        let mut h = Harness::new();
        h.attach(1, b"");
        h.assert_synced("tips off");
        h.c.sessions = vec![];
        h.open();
        h.rt.block_on(h.c.overlay_keys(b"\x1dd")).unwrap();
        assert_eq!(h.c.quit.as_deref(), Some("detached"));
        let mut h = Harness::new();
        h.attach(1, b"");
        h.open();
        h.rt.block_on(h.c.overlay_keys(b"\x18\x03")).unwrap();
        assert_eq!(h.c.quit.as_deref(), Some("detached"));
    }

    #[test]
    fn overlay_open_close_inside_alt_app_then_leave() {
        // pico: shell content, app enters alt, draws, the switcher is opened and closed, app exits.
        let mut h = Harness::new();
        h.attach(1, b"");
        h.out(b"user@host:~$ pico notes\r\n");
        h.out(b"\x1b[?1049h\x1b[H\x1b[2J GNU nano  New Buffer\x1b[5;1HHELP");
        h.assert_synced("in pico");
        h.open();
        h.close();
        h.assert_synced("after overlay in pico");
        assert!(h.real_text().contains("GNU nano"));
        h.out(b"\x1b[?1049l");
        h.assert_synced("after leaving pico");
        assert!(h.real_text().contains("user@host:~$ pico notes"), "{}", h.real_text());
    }

    #[test]
    fn overlay_opened_on_main_then_app_enters_alt() {
        let mut h = Harness::new();
        h.attach(1, b"");
        h.out(b"$ pico f\r\n");
        h.open();
        h.out(b"\x1b[?1049h\x1b[H\x1b[2Jeditor"); // output while the overlay is up
        h.close();
        h.assert_synced("alt after overlay");
        h.out(b"\x1b[?1049l");
        h.assert_synced("left alt");
        assert!(h.real_text().contains("$ pico f"));
    }

    #[test]
    fn status_line_on_main_then_app_enters_alt() {
        let mut h = Harness::new();
        h.attach(1, b"");
        h.out(b"$ pico f\r\n");
        h.c.status("reconnecting to somewhere");
        h.flush();
        assert!(h.real_text().contains("reconnecting"));
        h.out(b"\x1b[?1049h\x1b[H\x1b[2Jeditor");
        h.out(b"\x1b[?1049l");
        h.assert_synced("status line must not come back");
        assert!(!h.real_text().contains("reconnecting"));
    }

    #[test]
    fn switch_between_alt_sessions_and_to_main() {
        let mut h = Harness::new();
        h.attach(1, b"$ first\r\n");
        h.out(b"$ pico a\r\n");
        h.open();
        // session 2 is inside an alt-screen app
        h.attach(2, b"\x1b[?1049h\x1b[H\x1b[2Jtwo-editor");
        h.assert_synced("session 2 in alt");
        assert!(h.real_text().contains("two-editor"));
        h.open();
        h.attach(3, b"\x1b[?1049h\x1b[H\x1b[2Jthree-editor");
        h.assert_synced("session 3 in alt");
        h.out(b"\x1b[?1049l");
        h.assert_synced("session 3 left alt");
        assert!(!h.real_text().contains("$ first") && !h.real_text().contains("pico a"), "{}", h.real_text());
        // alt session -> main session
        h.out(b"\x1b[?1049h\x1b[H\x1b[2Jagain");
        h.open();
        h.attach(4, b"$ main-four\r\n");
        h.assert_synced("alt -> main session");
        assert!(h.real_text().contains("main-four") && !h.real_text().contains("again"));
    }

    #[test]
    fn repeated_overlay_in_main_leaves_nothing() {
        let mut h = Harness::new();
        h.attach(1, b"");
        h.out(b"$ ls\r\nfile\r\n");
        h.open();
        h.close();
        h.assert_synced("after overlay on main");
        h.out(b"\x1b[?1049h\x1b[H\x1b[2Jvim");
        h.open();
        h.close();
        h.open();
        h.close();
        h.out(b"\x1b[?1049l");
        h.assert_synced("end");
        assert!(h.real_text().contains("file"));
    }

    #[test]
    fn exited_screen_keys() {
        assert_eq!(exited_key(b"\r", 0), ExitedKey::Respawn);
        assert_eq!(exited_key(b"\x0b", 0), ExitedKey::Close);
        assert_eq!(exited_key(b"\x1bk", 0), ExitedKey::Close); // M-k
        assert_eq!(exited_key(b"ab\x1bk", 2), ExitedKey::Close);
        assert_eq!(exited_key(b"\x1b", 0), ExitedKey::Ignore); // a lone ESC
        assert_eq!(exited_key(b"\x1bj", 0), ExitedKey::Ignore);
        assert_eq!(exited_key(b"k", 0), ExitedKey::Ignore);
        assert_eq!(exited_key(b"y", 0), ExitedKey::Ignore);
        assert!(EXIT_BAR.ends_with("RET new shell · C-k close"));
    }

    #[test]
    fn ls_lines_fit_the_terminal_width() {
        let long = "#1   webshop-tests            ✗ make test (2) · 1m ago · tests/test_cart.py:41: AssertionError running tests for webshop";
        let cut = fit_line(long, Some(80));
        assert_eq!(cut.chars().count(), 80, "{cut}");
        assert!(cut.starts_with("#1   webshop-tests") && cut.ends_with('…'));
        assert_eq!(fit_line(long, None), long);
        assert_eq!(fit_line("#1 short", Some(80)), "#1 short");
    }

    #[test]
    fn ls_prints_one_line_per_session() {
        let s = SessionInfo {
            id: 3,
            name: "nt-reconnect".into(),
            name_source: crate::proto::NameSource::Llm,
            created_unix: 0,
            last_active_unix: 90,
            last_used_unix: 90,
            clients: 0,
            cols: 80,
            rows: 24,
            cwd: "~/src/nt".into(),
            fg_cmd: "make".into(),
            recap: "nt reconnect logic".into(),
            summary: "Long summary.".into(),
            exited: false,
            status: "⏵ make deploy · {dur}".into(),
            status_since: 0,
            cpu_pct: 0.0,
            rss_bytes: 0,
            pending: false,
        };
        let line = ls_entry(&s, 100);
        assert!(!line.contains('\n') && line.starts_with("#3") && line.contains("nt-reconnect"), "{line}");
        assert!(line.contains("⏵ make deploy · 1m40s") && line.ends_with("nt reconnect logic") && !line.contains("Long summary"), "{line}");
        let line = ls_entry(&SessionInfo { exited: true, recap: String::new(), ..s }, 100);
        assert!(line.ends_with("exited"), "{line}");
    }

    #[test]
    fn help_overlay_closes_on_any_key_and_repaints() {
        for (cols, rows) in [(80u16, 24u16), (40, 10)] {
            let mut h = Harness::new();
            h.c.cols = cols;
            h.c.rows = rows;
            h.c.mirror = vt100::Parser::new(rows, cols, 0);
            h.real = vt100::Parser::new(rows, cols, 0);
            h.out(b"user@host:~$ ls");
            h.c.prefix_pending = false;
            h.rt.block_on(h.c.prefix_cmd(b'?')).unwrap();
            h.flush();
            assert!(h.c.help && h.real_text().contains("C-] C-]"), "{cols}x{rows}\n{}", h.real_text());
            h.rt.block_on(h.c.on_input(b"x")).unwrap();
            h.flush();
            assert!(!h.c.help);
            h.assert_synced("after help");
            assert!(!h.real_text().contains("keys"));
        }
    }

    #[test]
    fn help_inside_switcher_returns_to_switcher() {
        let mut h = Harness::new();
        h.attach(1, b"");
        h.open();
        h.rt.block_on(h.c.overlay_keys(b"\x1d?")).unwrap();
        h.flush();
        assert!(h.c.help && h.real_text().contains("any key closes"));
        h.rt.block_on(h.c.on_input(b"q")).unwrap();
        h.flush();
        assert!(!h.c.help && matches!(h.c.mode, Mode::Switcher(_)));
        h.close();
        h.assert_synced("after closing the switcher");
    }
}

