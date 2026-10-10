//! The daemon: unix control socket, session table, per-connection handling.
//! Everything runs on one thread, so shared state is `Rc<RefCell<_>>`; borrows are never held across `.await`.

mod api;
mod groups;
mod guard;
#[cfg(feature = "llm")]
mod llm;
mod namer;
mod privacy;
mod prompt;
mod naming;
mod resources;
mod session;
mod shell;
mod transcript;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use nix::sys::signal::Signal;
use tokio::io::unix::AsyncFd;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream, UnixListener};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Notify;
use tokio::task::{spawn_local, LocalSet};

use crate::config::{self, Config};
use crate::proto::{self, FrameReader, FrameWriter, NameSource, SessionId, SessionInfo, C2S, PROTO_VERSION, S2C};
use session::{now_unix, Session};

type Shared = Rc<RefCell<State>>;
type Master = Rc<AsyncFd<OwnedFd>>;

/// A client falling further behind than this many queued output bytes gets a Redraw instead.
const OUTBOX_LIMIT: usize = 2 << 20;
/// Remote registrations expire after this long unused.
const REG_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// New transcript bytes needed before a session gets a new recap.
const NAMER_MIN_NEW: u64 = 1024;
/// Opening the switcher refreshes a summary after this much new output (a Ctrl-C'd build prints much less than 1 KiB).
const NAMER_MIN_NEW_OPEN: u64 = 256;
/// After a failed job (or batch) the session (or everything) is left alone for this long.
const NAMER_BACKOFF: Duration = Duration::from_secs(5 * 60);
const NAMER_MAX_JOBS: usize = 4;
const STATUS_MIN_GAP: Duration = Duration::from_secs(1);
const RES_WATCHED: Duration = Duration::from_secs(2);
/// Delay of the first CPU figure after a switcher opens.
const FIRST_CPU: Duration = Duration::from_millis(700);
const RES_IDLE: Duration = Duration::from_secs(30);
const STATUS_TICK: Duration = Duration::from_millis(250);
const TRANSCRIPT_FLUSH: Duration = Duration::from_secs(3);

/// Per-client outgoing queue with a bounded amount of queued terminal output.
struct Outbox {
    q: RefCell<VecDeque<S2C>>,
    bytes: Cell<usize>,
    overflow: Cell<bool>,
    notify: Notify,
}

fn payload_len(m: &S2C) -> usize {
    match m {
        S2C::Output { data, .. } | S2C::Redraw { data, .. } => data.len(),
        _ => 0,
    }
}

impl Outbox {
    fn new() -> Rc<Self> {
        Rc::new(Outbox {
            q: RefCell::new(VecDeque::new()),
            bytes: Cell::new(0),
            overflow: Cell::new(false),
            notify: Notify::new(),
        })
    }

    fn push(&self, m: S2C) {
        let n = payload_len(&m);
        if n > 0 && self.bytes.get() + n > OUTBOX_LIMIT {
            // Too far behind: drop all queued terminal output; the caller sends a Redraw.
            self.q.borrow_mut().retain(|m| payload_len(m) == 0);
            self.bytes.set(0);
            self.overflow.set(true);
            return;
        }
        self.bytes.set(self.bytes.get() + n);
        let mut q = self.q.borrow_mut();
        if matches!(m, S2C::Sessions(_)) {
            // A newer list supersedes an unsent one: a slow link must not queue up stale CPU numbers.
            q.retain(|m| !matches!(m, S2C::Sessions(_)));
        }
        q.push_back(m);
        drop(q);
        self.notify.notify_one();
    }

    fn pop(&self) -> Option<S2C> {
        let m = self.q.borrow_mut().pop_front()?;
        self.bytes.set(self.bytes.get().saturating_sub(payload_len(&m)));
        Some(m)
    }
}

struct ClientConn {
    tx: Rc<Outbox>,
    session: Option<SessionId>,
    cols: u16,
    rows: u16,
    /// Remote clients: registration id. Local (control socket) clients: None.
    ident: Option<[u8; 16]>,
    /// Last input seq applied, for local clients (remote ones keep it in their registration).
    local_seq: u64,
    kick: Rc<Notify>,
    /// The client's switcher is open.
    watching: bool,
}

struct Registration {
    key: [u8; 32],
    last_used: Instant,
    last_input_seq: u64,
    /// Connection currently using this registration; a reconnect kicks it.
    active: Option<u64>,
}

/// What a running naming job was built from: transcript position, context, commands covered, unix time, branch.
struct Inflight {
    mark: u64,
    ctx: String,
    seq: u64,
    asof: u64,
    running: bool,
    branch: String,
}

struct State {
    cfg: Config,
    sessions: BTreeMap<SessionId, Session>,
    clients: HashMap<u64, ClientConn>,
    next_session: SessionId,
    next_client: u64,
    regs: HashMap<[u8; 16], Registration>,
    /// Transcript directory (None = transcripts disabled).
    tdir: Option<PathBuf>,
    namer_running: bool,
    /// When the last batch started / ended.
    namer_last: Option<Instant>,
    namer_ended: Option<Instant>,
    /// Woken when a switcher opens: run a batch now if sessions are dirty.
    namer_kick: Rc<Notify>,
    /// A switcher opened while a batch was running (or more sessions were due than one batch holds):
    /// run another one as soon as this one ends.
    namer_again: bool,
    /// The resource guards held back a run for an open switcher: try again on the next tick while it stays open.
    gated_open: bool,
    namer_backoff: Option<Instant>,
    /// The last naming failure, shown in the switcher and `nt ls` (empty = none).
    naming_error: String,
    /// Why runs are postponed right now (low memory, battery); logged once per episode.
    gate_msg: Option<String>,
    /// Sessions in the running batch: transcript mark and context at job creation.
    inflight: HashMap<SessionId, Inflight>,
    /// Sessions due for a summary that did not fit in the running batch (shown as pending too).
    queued: HashSet<SessionId>,
    /// Group keys as last seen on the naming tick, to push a change (cwd moves) to the clients.
    seen_groups: Vec<(SessionId, Option<String>)>,
    sampler: resources::Sampler,
    sampled_at: Option<Instant>,
}

enum NamerMsg {
    Out(namer::Out),
    Done(anyhow::Result<()>),
}

impl State {
    fn send(&self, cid: u64, msg: S2C) {
        if let Some(c) = self.clients.get(&cid) {
            c.tx.push(msg);
        }
    }

    fn register(&mut self) -> S2C {
        self.regs.retain(|_, r| r.last_used.elapsed() < REG_TTL);
        let (mut id, mut key) = ([0u8; 16], [0u8; 32]);
        getrandom::fill(&mut id).expect("getrandom");
        getrandom::fill(&mut key).expect("getrandom");
        self.regs.insert(id, Registration { key, last_used: Instant::now(), last_input_seq: 0, active: None });
        S2C::Registered { port: self.cfg.port, client_id: id, key }
    }

    /// Key of a live registration (and mark it used).
    fn lookup_key(&mut self, id: &[u8; 16]) -> Option<[u8; 32]> {
        let r = self.regs.get_mut(id)?;
        if r.last_used.elapsed() >= REG_TTL {
            self.regs.remove(id);
            return None;
        }
        r.last_used = Instant::now();
        Some(r.key)
    }

    /// True if `seq` is new for this client's identity (and records it).
    fn accept_seq(&mut self, cid: u64, seq: u64) -> bool {
        let Some(c) = self.clients.get_mut(&cid) else { return false };
        let last = match c.ident.and_then(|id| self.regs.get_mut(&id)) {
            Some(r) => &mut r.last_input_seq,
            None => &mut c.local_seq,
        };
        if seq <= *last {
            return false;
        }
        *last = seq;
        true
    }

    fn attached(&self, sid: SessionId) -> impl Iterator<Item = u64> + '_ {
        self.clients.iter().filter(move |(_, c)| c.session == Some(sid)).map(|(id, _)| *id)
    }

    fn infos(&self) -> Vec<SessionInfo> {
        let mut v: Vec<SessionInfo> = self
            .sessions
            .values()
            .map(|s| {
                let mut i = s.info(self.attached(s.id).count() as u32);
                i.pending = self.inflight.contains_key(&s.id) || self.queued.contains(&s.id);
                i
            })
            .collect();
        let mut groups = groups::assign(&v.iter().map(|i| (i.id, i.cwd.clone())).collect::<Vec<_>>(), &home_dir());
        for i in &mut v {
            i.group = groups.remove(&i.id);
        }
        v
    }

    /// Did a session move to another group since the last call? (Groups follow the cwd; this runs on the naming tick.)
    fn groups_changed(&mut self) -> bool {
        let now: Vec<(SessionId, Option<String>)> = {
            let mut v: Vec<_> = self.infos().into_iter().map(|i| (i.id, i.group.map(|g| g.key))).collect();
            v.sort();
            v
        };
        let old = std::mem::replace(&mut self.seen_groups, now.clone());
        // A session that moved to another group (or into one) is doing something else now: summarize it again.
        for (id, key) in &now {
            if old.iter().any(|(i, _)| i == id) && !old.contains(&(*id, key.clone()))
                && let Some(s) = self.sessions.get_mut(id)
            {
                s.dirty = true;
                s.ctx_dirty = true;
            }
        }
        old != now
    }

    fn broadcast_paused(&self) {
        let why = self.gate_msg.as_deref().map_or("", guard::short_reason).to_string();
        for c in self.clients.values() {
            c.tx.push(S2C::NamerPaused(why.clone()));
        }
    }

    /// The session the watching switcher's client is attached to.
    fn watcher_current(&self) -> Option<Option<SessionId>> {
        let mut w = self.clients.values().filter(|c| c.watching);
        w.next().map(|c| c.session)
    }

    fn broadcast_error(&self) {
        for c in self.clients.values() {
            c.tx.push(S2C::NamerError(self.naming_error.clone()));
        }
    }

    fn broadcast_sessions(&self) {
        let infos = self.infos();
        for c in self.clients.values() {
            c.tx.push(S2C::Sessions(infos.clone()));
        }
    }

    /// Sample CPU/RSS now. Returns true if a client is watching (so the new numbers should be pushed).
    fn sample_resources(&mut self) -> bool {
        let now = Instant::now();
        let watched = self.clients.values().any(|c| c.watching);
        let every = if watched { RES_WATCHED } else { RES_IDLE };
        if self.sampled_at.is_some_and(|t| now.duration_since(t) < every) {
            return false;
        }
        self.sampled_at = Some(now);
        self.apply_sample(now, false);
        // The branch is re-read while the switcher is open (a `git checkout` in another terminal).
        if watched {
            for s in self.sessions.values_mut() {
                s.note_cwd();
            }
        }
        watched
    }

    /// Take a /proc sample and store it per session. `no_cpu`: the window is too long for a meaningful CPU figure.
    fn apply_sample(&mut self, now: Instant, no_cpu: bool) {
        let m = self.sampler.sample(now);
        for s in self.sessions.values_mut() {
            let (cpu, rss) = if s.exited { (0.0, 0) } else { m.get(&s.child.id()).copied().unwrap_or((0.0, 0)) };
            s.cpu_pct = if no_cpu { 0.0 } else { cpu };
            s.rss_bytes = rss;
        }
    }

    /// A switcher was opened: refresh what the idle 30 s sampling left stale. The memory and the branches are read at once;
    /// the CPU baseline is taken now and the first real figure follows after `FIRST_CPU` instead of a 2 s wait.
    fn on_watch(&mut self) {
        for s in self.sessions.values_mut() {
            s.note_cwd();
        }
        let now = Instant::now();
        if self.sampled_at.is_none_or(|t| now.duration_since(t) >= RES_WATCHED) {
            self.apply_sample(now, true);
            self.sampled_at = Some(now.checked_sub(RES_WATCHED - FIRST_CPU).unwrap_or(now));
        }
    }

    /// Statuses that changed since they were last pushed, at most once a second per session.
    /// Returns true if a push is due.
    fn status_changed(&mut self) -> bool {
        let mut any = false;
        for s in self.sessions.values_mut() {
            if s.pushed_at.is_some_and(|t| t.elapsed() < STATUS_MIN_GAP) {
                continue;
            }
            let cur = s.status();
            if cur != s.pushed_status {
                s.pushed_status = cur;
                s.pushed_at = Some(Instant::now());
                any = true;
            }
        }
        any
    }

    /// Heuristic naming pass for sessions with recent output. Returns true if any name changed.
    fn heuristic_names(&mut self) -> bool {
        let now = now_unix();
        let mut changed = false;
        for s in self.sessions.values_mut() {
            changed |= s.note_cwd();
            let recent = now.saturating_sub(s.last_active) <= naming::RECENT_SECS;
            if !recent || s.exited {
                continue;
            }
            let Some(p) = naming::probe(s.master.as_ref().get_ref(), s.child.id()) else { continue };
            let name = p.name();
            if naming::heuristic_may_override(s.name_source, &s.name, s.llm_ctx.as_deref(), &p.ctx, &name) {
                s.name = name;
                s.name_source = NameSource::Heuristic;
                s.save_meta();
                changed = true;
            }
        }
        changed
    }

    /// Jobs for the next namer batch, if one is due (see `namer::batch_allowed` / `session_ready`); never two at once.
    /// `force`: the switcher was just opened.
    fn namer_jobs(&mut self, force: bool) -> Vec<namer::Job> {
        let now = Instant::now();
        let eager = namer_now();
        if self.cfg.llm == config::LlmMode::Off || self.namer_backoff.is_some_and(|t| now < t) {
            return Vec::new();
        }
        if self.namer_running {
            // The sessions that are due now run right after this batch.
            self.namer_again |= force;
            return Vec::new();
        }
        if !eager && !namer::batch_allowed(force, self.namer_last.map(|t| now.duration_since(t))) {
            return Vec::new();
        }
        let unix = now_unix();
        let mut cands: Vec<(namer::Cand, naming::Probe)> = Vec::new();
        for s in self.sessions.values() {
            let new = s.tw.written().saturating_sub(s.recap_mark);
            // A branch/group change is news by itself; otherwise there must be new output. The per-session backoff after a
            // failed run does not hold back a switcher that was just opened.
            if (new == 0 && !s.ctx_dirty) || (!eager && !force && s.backoff_until > unix) || s.exited {
                continue;
            }
            // A rolling update needs something new to say.
            if s.log.integrated && !s.summary.is_empty() && !s.ctx_dirty && !s.log.has_news(s.summary_seq) {
                continue;
            }
            let Some(p) = naming::probe(s.master.as_ref().get_ref(), s.child.id()) else { continue };
            let ctx_changed = s.recap_ctx.as_deref().is_some_and(|c| c != p.ctx) || s.ctx_dirty;
            // Opening the switcher refreshes every session with any activity since its summary.
            let min_new = if force { NAMER_MIN_NEW_OPEN } else { NAMER_MIN_NEW };
            let stale = force && !s.summary.is_empty() && s.summary_stale();
            let dirty = s.dirty || new >= min_new || ctx_changed || stale;
            let quiet = Duration::from_secs(unix.saturating_sub(s.last_active));
            if eager || namer::session_ready(dirty, quiet, force) {
                cands.push((namer::Cand { sid: s.id, new, last_used: s.last_used }, p));
            }
        }
        if cands.is_empty() {
            self.gate_msg = None;
        } else if self.cfg.llm == config::LlmMode::Local {
            self.gated_open = false;
            match guard::check_system(&self.cfg.llm_model, self.cfg.llm_on_battery) {
                Err(why) => {
                    if self.gate_msg.is_none() {
                        eprintln!("nt: naming postponed: {why}");
                    }
                    self.gate_msg = Some(why);
                    self.gated_open = force || (self.gated_open && self.clients.values().any(|c| c.watching));
                    return Vec::new();
                }
                Ok(()) => self.gate_msg = None,
            }
        }
        // Watched: the switcher's current session first, then in the switcher's order.
        let watcher = force.then(|| self.watcher_current()).flatten();
        let mut order: Vec<namer::Cand> = cands.iter().map(|c| c.0).collect();
        namer::order_cands(&mut order, if force { Some(watcher.flatten()) } else { None });
        let rank = |sid| order.iter().position(|c| c.sid == sid).unwrap_or(usize::MAX);
        cands.sort_by_key(|c| rank(c.0.sid));
        if cands.len() > NAMER_MAX_JOBS && force {
            self.namer_again = true;
            self.queued = cands[NAMER_MAX_JOBS..].iter().map(|c| c.0.sid).collect();
        } else {
            self.queued.clear();
        }
        cands.truncate(NAMER_MAX_JOBS);
        let mut jobs = Vec::new();
        let ident = privacy::Identity::current();
        let home = ident.home.clone();
        for (c, p) in cands {
            let (new, sid) = (c.new, c.sid);
            let s = self.sessions.get_mut(&sid).unwrap();
            let mut header = shell::header(&p.cwd, &home);
            let branch = shell::branch_of(&p.cwd).unwrap_or_default();
            if !s.summary.is_empty() && s.summary_branch != branch {
                // The topic describes what is going on NOW, so say what changed.
                let (a, b) = (if s.summary_branch.is_empty() { "(none)" } else { &s.summary_branch }, if branch.is_empty() { "(none)" } else { &branch });
                header.push_str(&format!("\nbranch changed since the earlier summary: {a} -> {b}"));
            }
            let fg = p.cmd.clone().unwrap_or_default();
            // Live-state phrases of an older summary ("currently building") must not be carried forward.
            let prev = (!s.summary.is_empty()).then(|| namer::drop_live_state(&s.summary, false).unwrap_or_else(|| s.summary.clone()));
            let text = if s.log.integrated {
                let fg = s.log.running_cmd().map_or(fg, |c| c.cmd.clone());
                s.log.digest(&header, &fg, &home, unix, prev.as_deref().map(|p| (p, s.summary_seq)))
            } else {
                namer::guess_digest(&header, &s.tw.tail(new.min(8192) as usize), &fg, prev.as_deref())
            };
            // The model never sees the home directory, the login name or the host name.
            let text = ident.scrub(&text);
            s.dirty = false;
            s.ctx_dirty = false;
            self.inflight.insert(sid, Inflight { mark: s.tw.written(), ctx: p.ctx.clone(), seq: s.log.covered_seq(), asof: unix, running: s.log.is_running(), branch });
            jobs.push(namer::Job { sid, text });
        }
        if !jobs.is_empty() {
            self.namer_running = true;
            self.namer_last = Some(now);
        }
        jobs
    }

    /// Apply one result of the running batch (`None`: the batch ended without a result for `sid`).
    /// Returns true if anything visible changed.
    fn apply_one(&mut self, sid: SessionId, out: Option<&namer::Out>) -> bool {
        let Some(Inflight { mark, ctx, seq, asof, running: was_running, branch }) = self.inflight.remove(&sid) else { return false };
        let Some(s) = self.sessions.get_mut(&sid) else { return false };
        let mut changed = true; // at least the pending marker goes away
        if out.is_some_and(|o| o.recap.is_some() || o.summary.is_some()) {
            self.naming_error.clear();
            if let Some(r) = out.and_then(|o| o.recap.as_ref()) {
                s.recap = r.clone();
            }
            // The status line shows live state; a summary that claims a finished command is still going is dropped.
            let running = s.log.is_running() || s.status().0.starts_with(['⏵', '✎']);
            if let Some(sm) = out.and_then(|o| o.summary.as_ref()).and_then(|sm| namer::drop_live_state(sm, running)) {
                s.summary = sm;
                s.summary_seq = seq;
                s.summary_asof = asof;
                s.summary_running = was_running;
                s.summary_branch = branch;
            }
            s.recap_mark = mark;
            s.recap_ctx = Some(ctx.clone());
        } else {
            s.backoff_until = now_unix() + NAMER_BACKOFF.as_secs();
        }
        if let Some(name) = out.and_then(|o| o.name.as_ref())
            && s.name_source != NameSource::User
            && *name != s.name
        {
            s.name = name.clone();
            s.name_source = NameSource::Llm;
            s.llm_ctx = Some(ctx);
            changed = true;
        }
        s.save_meta();
        changed
    }

    /// The batch process ended. Returns true if anything visible changed.
    fn finish_batch(&mut self, res: anyhow::Result<()>) -> bool {
        self.namer_running = false;
        self.namer_ended = Some(Instant::now());
        if let Err(e) = res {
            eprintln!("nt: namer failed: {e:#}");
            self.naming_error = format!("{e:#}");
            self.namer_backoff = Some(Instant::now() + NAMER_BACKOFF);
        }
        let left: Vec<SessionId> = self.inflight.keys().copied().collect();
        let mut changed = false;
        for sid in left {
            changed |= self.apply_one(sid, None);
        }
        changed
    }

    fn create_session(&mut self, cols: u16, rows: u16, term: &str, cwd: Option<&str>) -> Result<SessionId> {
        let taken: Vec<&str> = self.sessions.values().map(|s| s.name.as_str()).collect();
        let name = naming::initial_name(&taken);
        let id = self.next_session;
        let mut s = Session::spawn(id, name, cols.max(1), rows.max(1), term, self.cfg.scrollback, self.tdir.as_deref(), self.cfg.shell_integration, cwd)?;
        s.title_topic = self.cfg.llm == config::LlmMode::Off;
        self.next_session += 1;
        self.sessions.insert(id, s);
        Ok(id)
    }

    fn on_output(&mut self, sid: SessionId, data: &[u8]) {
        let Some(s) = self.sessions.get_mut(&sid) else { return };
        let offset = s.ring.end();
        s.feed(data);
        let redraw = S2C::Redraw { offset: s.ring.end(), data: s.redraw() };
        for cid in self.attached(sid) {
            let tx = &self.clients[&cid].tx;
            tx.push(S2C::Output { offset, data: data.to_vec() });
            if tx.overflow.replace(false) {
                tx.push(redraw.clone());
            }
        }
    }

    /// The shell process ended. A session being closed ends with it; otherwise it becomes `exited`
    /// and stays (screen, transcript and name) until the user closes or respawns it.
    fn shell_exited(&mut self, sid: SessionId) {
        let Some(s) = self.sessions.get_mut(&sid) else { return };
        s.signal(Signal::SIGHUP);
        if s.closing {
            return self.end_session(sid);
        }
        s.mark_exited();
        s.save_meta();
        self.broadcast_sessions();
    }

    /// The session is closed for good: tell attached clients and forget it.
    fn end_session(&mut self, sid: SessionId) {
        let Some(mut s) = self.sessions.remove(&sid) else { return };
        s.tw.finish();
        s.save_meta();
        let _ = s.child.kill();
        let _ = s.child.wait();
        let ids: Vec<u64> = self.attached(sid).collect();
        for cid in ids {
            self.send(cid, S2C::SessionEnded { session: sid });
            self.clients.get_mut(&cid).unwrap().session = None;
        }
        self.broadcast_sessions();
    }

    /// PTY handle to write client input to; also applies the "last active client sets the size" rule.
    fn input_target(&mut self, cid: u64) -> Option<Master> {
        let c = self.clients.get(&cid)?;
        let (sid, cols, rows) = (c.session?, c.cols, c.rows);
        let s = self.sessions.get_mut(&sid)?;
        if s.exited {
            return None;
        }
        s.resize(cols, rows);
        s.last_active = now_unix();
        s.last_used = s.last_active;
        Some(s.master.clone())
    }

    fn remove_client(&mut self, cid: u64) {
        if let Some(c) = self.clients.remove(&cid)
            && let Some(r) = c.ident.and_then(|id| self.regs.get_mut(&id))
            && r.active == Some(cid)
        {
            r.active = None;
        }
        self.broadcast_sessions();
    }
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// Hidden debug command: `nt name-test <file>...` summarizes text files the way the namer does, in one batch
/// (one model load, one prefix evaluation). A file that already is a digest (starts with `cwd:`; a leading
/// `# gold:` line is dropped) is used as is. Prints name, topic, timings and peak RSS.
pub fn name_test(cfg: &Config, files: &[String], cwd: Option<&str>) -> Result<()> {
    if files.is_empty() {
        bail!("usage: nt name-test <file>...");
    }
    namer::lower_priority();
    let home = std::env::var("HOME").unwrap_or_default();
    let cwd = cwd.map(String::from).unwrap_or_else(|| std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default());
    let mut jobs = Vec::new();
    for (i, f) in files.iter().enumerate() {
        let text = std::fs::read_to_string(f).with_context(|| f.clone())?;
        let text: String = text.lines().filter(|l| !l.starts_with("# gold:")).collect::<Vec<_>>().join("\n");
        let digest = if text.trim_start().starts_with("cwd:") || text.trim_start().starts_with("earlier summary:") {
            text + "\n"
        } else {
            namer::guess_digest(&shell::header(Path::new(&cwd), &home), &text, "", None)
        };
        let digest = privacy::Identity::current().scrub(&digest);
        eprintln!("[{i}] {f}\n{digest}");
        jobs.push(namer::Job { sid: i as SessionId, text: digest });
    }
    let show = |j: &namer::Job, r: api::Answer| match r {
        Ok(r) => println!(
            "[{}] name: {}\n    topic: {}\n    summary: {}",
            j.sid,
            r.name.as_deref().unwrap_or("-"),
            r.topic.as_deref().unwrap_or("-"),
            r.summary.as_deref().unwrap_or("-")
        ),
        Err(e) => println!("[{}] error: {e:#}", j.sid),
    };
    match cfg.llm {
        config::LlmMode::Api => {
            let t = std::time::Instant::now();
            api::run(cfg, &jobs, show)?;
            eprintln!("api total: {:.2?}", t.elapsed());
        }
        config::LlmMode::Local => {
            #[cfg(feature = "llm")]
            llm::run(cfg, &jobs, |l| eprintln!("{l}"), show)?;
            #[cfg(not(feature = "llm"))]
            bail!("built without the `llm` feature");
        }
        config::LlmMode::Off => bail!("llm is off in the config"),
    }
    let rss = std::fs::read_to_string("/proc/self/status")?;
    eprintln!("{}", rss.lines().filter(|l| l.starts_with("VmHWM") || l.starts_with("Threads")).collect::<Vec<_>>().join("  "));
    Ok(())
}

/// Hidden: `nt namer` (spawned by the daemon).
pub fn namer(cfg: &Config) -> Result<()> {
    namer::namer_main(cfg)
}

pub fn run(cfg: Config, foreground: bool, restart: bool) -> Result<()> {
    let dir = config::runtime_dir()?;
    let sock = dir.join("ctl.sock");
    if restart {
        stop_running(&sock)?;
    }
    if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
        bail!("server already running ({})", sock.display());
    }
    if !foreground {
        daemonize(&dir)?;
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    LocalSet::new().block_on(&rt, serve(cfg, &sock))
}

/// `nt server --restart`: stop the daemon on `sock`, whatever its version, and wait until it is gone. SIGTERM is
/// the graceful path (the daemon removes its socket and exits; shells get SIGHUP as the ptys close). The pid comes
/// from the socket peer credentials, so this works against daemons that do not understand any request.
fn stop_running(sock: &Path) -> Result<()> {
    use nix::sys::signal::{kill, Signal};
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    use std::os::unix::net::UnixStream;
    let Ok(s) = UnixStream::connect(sock) else { return Ok(()) };
    let pid = nix::unistd::Pid::from_raw(getsockopt(&s, PeerCredentials)?.pid());
    drop(s);
    eprintln!("nt: stopping the running daemon (pid {pid})");
    kill(pid, Signal::SIGTERM)?;
    let gone = |t: u64| {
        for _ in 0..t * 20 {
            if UnixStream::connect(sock).is_err() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    };
    if !gone(10) {
        eprintln!("nt: daemon did not exit, killing it");
        let _ = kill(pid, Signal::SIGKILL);
        let _ = std::fs::remove_file(sock);
        if !gone(5) {
            bail!("could not stop the running daemon (pid {pid})");
        }
    }
    Ok(())
}

/// Classic fork + setsid; stdin from /dev/null, stdout/stderr to `server.log`.
fn daemonize(dir: &Path) -> Result<()> {
    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("server.log"))?;
    let null = std::fs::File::open("/dev/null")?;
    // SAFETY: still single-threaded here (no runtime yet).
    match unsafe { nix::unistd::fork() }? {
        nix::unistd::ForkResult::Parent { .. } => std::process::exit(0),
        nix::unistd::ForkResult::Child => {}
    }
    nix::unistd::setsid()?;
    nix::unistd::dup2_stdin(&null)?;
    nix::unistd::dup2_stdout(&log)?;
    nix::unistd::dup2_stderr(&log)?;
    Ok(())
}

async fn serve(cfg: Config, sock: &Path) -> Result<()> {
    let _ = std::fs::remove_file(sock);
    let listener = UnixListener::bind(sock).context("binding control socket")?;
    std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o600))?;
    eprintln!("nt server listening on {}", sock.display());

    let port = cfg.port;
    let st = Rc::new(RefCell::new(State {
        cfg,
        regs: HashMap::new(),
        sessions: BTreeMap::new(),
        clients: HashMap::new(),
        next_session: 1,
        next_client: 1,
        tdir: transcript::dir(),
        namer_running: false,
        namer_last: None,
        namer_ended: None,
        namer_kick: Rc::new(Notify::new()),
        namer_again: false,
        gated_open: false,
        namer_backoff: None,
        naming_error: String::new(),
        gate_msg: None,
        inflight: HashMap::new(),
        queued: HashSet::new(),
        seen_groups: Vec::new(),
        sampler: resources::Sampler::default(),
        sampled_at: None,
    }));
    match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => {
            eprintln!("nt server listening on tcp port {}", l.local_addr()?.port());
            spawn_local(tcp_loop(l, st.clone()));
        }
        Err(e) => eprintln!("cannot listen on tcp port {port} (remote access disabled): {e}"),
    }
    spawn_local(naming_loop(st.clone()));
    spawn_local(transcript_flush_loop(st.clone()));
    spawn_local(status_loop(st.clone()));
    spawn_local(resource_loop(st.clone()));
    let mut term = signal(SignalKind::terminate())?;
    loop {
        tokio::select! {
            r = listener.accept() => {
                let (stream, _) = r?;
                let (r, w) = stream.into_split();
                let (r, w) = (FrameReader::new(Box::new(r), None), FrameWriter::new(Box::new(w), None));
                spawn_local(handle_conn(r, w, st.clone(), None));
            }
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    let _ = std::fs::remove_file(sock);
    Ok(())
}

/// Debug aid (`NT_NAMER_NOW=1` in the daemon's environment): run a naming batch as soon as a session has any new output,
/// without the 5-minute and minimum-output limits. Used to prepare the demo recording.
fn namer_now() -> bool {
    std::env::var_os("NT_NAMER_NOW").is_some_and(|v| v != "0")
}

async fn naming_loop(st: Shared) {
    let (tx, mut results) = tokio::sync::mpsc::unbounded_channel::<NamerMsg>();
    let kick = st.borrow().namer_kick.clone();
    let mut tick = tokio::time::interval(if namer_now() { Duration::from_secs(1) } else { naming::INTERVAL });
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let force: bool;
        tokio::select! {
            _ = tick.tick() => {
                // A refresh the guards postponed for an open switcher is retried at once instead of waiting for quiet time.
                let s = st.borrow();
                force = s.gated_open && s.clients.values().any(|c| c.watching);
            }
            _ = kick.notified() => force = true,
            Some(m) = results.recv() => {
                let mut s = st.borrow_mut();
                let before = s.naming_error.clone();
                let done = matches!(m, NamerMsg::Done(_));
                let changed = match m {
                    NamerMsg::Out(o) => s.apply_one(o.sid, Some(&o)),
                    NamerMsg::Done(r) => s.finish_batch(r),
                };
                if changed {
                    s.broadcast_sessions();
                }
                if s.naming_error != before {
                    s.broadcast_error();
                }
                // Sessions that came due while the batch ran (or did not fit in it): a follow-up batch at once.
                if done && std::mem::take(&mut s.namer_again) && s.clients.values().any(|c| c.watching) {
                    force = true;
                } else {
                    continue;
                }
            }
        }
        let (jobs, cfg) = {
            let mut s = st.borrow_mut();
            if !force && (s.heuristic_names() | s.groups_changed()) {
                s.broadcast_sessions();
            }
            let gate = s.gate_msg.clone();
            let jobs = s.namer_jobs(force);
            if s.gate_msg != gate {
                s.broadcast_paused();
            }
            if !jobs.is_empty() {
                s.broadcast_sessions(); // shows the pending marker
            }
            (jobs, s.cfg.clone())
        };
        if !jobs.is_empty() {
            let tx = tx.clone();
            spawn_local(async move {
                let tx2 = tx.clone();
                let r = tokio::task::spawn_blocking(move || {
                    namer::run_batch(&cfg, &jobs, |o| {
                        let _ = tx2.send(NamerMsg::Out(o));
                    })
                })
                .await;
                let _ = tx.send(NamerMsg::Done(r.unwrap_or_else(|e| Err(anyhow::anyhow!("namer task: {e}")))));
            });
        }
    }
}

/// Pushes changed status lines to clients (the `{dur}` / `{ago}` counters run on the client).
async fn status_loop(st: Shared) {
    let mut tick = tokio::time::interval(STATUS_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let mut s = st.borrow_mut();
        if s.status_changed() {
            s.broadcast_sessions();
        }
    }
}

/// CPU/RSS sampling: every 2 s while a switcher is open, else every 30 s.
async fn resource_loop(st: Shared) {
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let mut s = st.borrow_mut();
        if s.sample_resources() {
            s.broadcast_sessions();
        }
    }
}

/// Transcripts are buffered in memory and written out every few seconds.
async fn transcript_flush_loop(st: Shared) {
    let mut tick = tokio::time::interval(TRANSCRIPT_FLUSH);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        for s in st.borrow_mut().sessions.values_mut() {
            s.tw.flush();
        }
    }
}

async fn tcp_loop(listener: TcpListener, st: Shared) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let st = st.clone();
        spawn_local(async move {
            if let Err(e) = tcp_accept(stream, st).await {
                eprintln!("tcp client rejected: {e:#}");
            }
        });
    }
}

/// Server side of the handshake. Unknown client ids are dropped before any reply.
async fn tcp_accept(stream: TcpStream, st: Shared) -> Result<()> {
    let _ = stream.set_nodelay(true);
    keepalive(&stream);
    let (mut r, mut w) = stream.into_split();
    let mut hello = [0u8; 32];
    tokio::time::timeout(HANDSHAKE_TIMEOUT, r.read_exact(&mut hello)).await.context("handshake timeout")??;
    let (id, nonce_c): ([u8; 16], [u8; 16]) = (hello[..16].try_into()?, hello[16..].try_into()?);
    let key = st.borrow_mut().lookup_key(&id).context("unknown client id")?;
    let mut nonce_s = [0u8; 16];
    getrandom::fill(&mut nonce_s).context("getrandom")?;
    tokio::io::AsyncWriteExt::write_all(&mut w, &nonce_s).await?;
    let (c2s, s2c) = proto::crypts(&key, &nonce_c, &nonce_s);
    let (r, w) = (FrameReader::new(Box::new(r), Some(c2s)), FrameWriter::new(Box::new(w), Some(s2c)));
    handle_conn(r, w, st, Some(id)).await;
    Ok(())
}

fn keepalive(s: &TcpStream) {
    use nix::sys::socket::{setsockopt, sockopt};
    let _ = setsockopt(s, sockopt::KeepAlive, &true);
    let _ = setsockopt(s, sockopt::TcpKeepIdle, &30);
    let _ = setsockopt(s, sockopt::TcpKeepInterval, &10);
    let _ = setsockopt(s, sockopt::TcpKeepCount, &3);
}

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

async fn handle_conn(rd: FrameReader, wr: FrameWriter, st: Shared, ident: Option<[u8; 16]>) {
    if let Err(e) = conn_loop(rd, wr, &st, ident).await {
        // A peer that connects and leaves without a Hello (e.g. a liveness probe) is not worth logging.
        let eof = e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::UnexpectedEof);
        if !eof {
            eprintln!("client error: {e:#}");
        }
    }
}

/// Shared by the unix control socket (plain, `ident = None`) and TCP (sealed, `ident = Some(client_id)`).
async fn conn_loop(mut rd: FrameReader, mut wr: FrameWriter, st: &Shared, ident: Option<[u8; 16]>) -> Result<()> {
    let (proto, hello) = tokio::time::timeout(HANDSHAKE_TIMEOUT, rd.read_hello()).await.context("no Hello")??;
    if hello.is_none() {
        // Tell the client why before closing; it shows the text and does not retry.
        let n = st.borrow().sessions.len();
        let (sessions, where_) = (if n == 1 { "1 session".to_string() } else { format!("{n} sessions") }, if ident.is_some() { " on the server host" } else { "" });
        let msg = format!(
            "nt version mismatch: server protocol {PROTO_VERSION}, client {proto}. A different nt version is running with {sessions}; run `nt server --restart`{where_} to replace it (this ends those sessions)"
        );
        eprintln!("client with protocol {proto} refused");
        wr.write_msg(&S2C::Error(msg)).await?;
        return Ok(());
    }
    let tx = Outbox::new();
    let kick = Rc::new(Notify::new());
    let cid = {
        let mut s = st.borrow_mut();
        let cid = s.next_client;
        s.next_client += 1;
        let mut last_seq = 0;
        if let Some(id) = ident
            && let Some(reg) = s.regs.get_mut(&id)
        {
            last_seq = reg.last_input_seq;
            // A reconnect replaces a stale connection of the same client.
            if let Some(old) = reg.active.replace(cid)
                && let Some(c) = s.clients.get(&old)
            {
                c.kick.notify_one();
            }
        }
        tx.push(S2C::Welcome { proto: PROTO_VERSION, last_input_seq: last_seq });
        s.clients.insert(
            cid,
            ClientConn { tx: tx.clone(), session: None, cols: 80, rows: 24, ident, local_seq: 0, kick: kick.clone(), watching: false },
        );
        cid
    };
    let r = tokio::select! {
        r = client_loop(&mut rd, st, cid) => r,
        r = writer_loop(&mut wr, &tx) => r,
        _ = kick.notified() => Ok(()),
    };
    st.borrow_mut().remove_client(cid);
    r
}

async fn writer_loop(wr: &mut FrameWriter, ob: &Outbox) -> Result<()> {
    loop {
        match ob.pop() {
            Some(m) => wr.write_msg(&m).await?,
            None => ob.notify.notified().await,
        }
    }
}

async fn client_loop(rd: &mut FrameReader, st: &Shared, cid: u64) -> Result<()> {
    // Any read error (including a failed decryption) just ends the connection.
    while let Ok(msg) = rd.read_msg::<C2S>().await {
        match msg {
            C2S::Hello { .. } => {}
            C2S::List => {
                let s = st.borrow();
                if !s.naming_error.is_empty() {
                    s.send(cid, S2C::NamerError(s.naming_error.clone()));
                }
                if let Some(g) = &s.gate_msg {
                    s.send(cid, S2C::NamerPaused(guard::short_reason(g).into()));
                }
                s.send(cid, S2C::Sessions(s.infos()));
            }
            C2S::Attach { session, cols, rows, term, resume_from, cwd } => {
                attach(st, cid, session, cols, rows, &term, resume_from, cwd.as_deref());
            }
            C2S::Watch(w) => {
                let mut s = st.borrow_mut();
                if let Some(c) = s.clients.get_mut(&cid) {
                    c.watching = w;
                }
                if w {
                    s.on_watch();
                    s.broadcast_sessions();
                    s.namer_kick.notify_one();
                } else if !s.queued.is_empty() && !s.clients.values().any(|c| c.watching) {
                    s.queued.clear();
                }
            }
            C2S::Detach => {
                st.borrow_mut().clients.get_mut(&cid).unwrap().session = None;
                st.borrow().broadcast_sessions();
            }
            C2S::Input { seq, data } => {
                if !st.borrow_mut().accept_seq(cid, seq) {
                    continue; // duplicate (resent after a reconnect)
                }
                let master = st.borrow_mut().input_target(cid);
                if data.iter().any(|&b| b == b'\r' || b == b'\n') {
                    let mut s = st.borrow_mut();
                    let sid = s.clients.get(&cid).and_then(|c| c.session);
                    if let Some(sess) = sid.and_then(|id| s.sessions.get_mut(&id)) {
                        sess.note_cwd();
                    }
                }
                if let Some(m) = master {
                    pty_write(&m, &data).await?;
                }
            }
            C2S::Resize { cols, rows } => {
                let mut s = st.borrow_mut();
                let c = s.clients.get_mut(&cid).unwrap();
                (c.cols, c.rows) = (cols, rows);
                if let Some(sid) = c.session
                    && let Some(sess) = s.sessions.get_mut(&sid)
                {
                    sess.resize(cols, rows);
                }
            }
            C2S::Search { query } => {
                let (dir, live) = {
                    let mut s = st.borrow_mut();
                    s.sessions.values_mut().for_each(|x| x.tw.flush());
                    (s.tdir.clone(), s.sessions.values().map(|x| (x.created, x.id, x.summary.clone())).collect::<Vec<_>>())
                };
                let hits = match dir {
                    Some(d) => tokio::task::spawn_blocking(move || transcript::search(&d, &live, &query)).await.unwrap_or_default(),
                    None => Vec::new(),
                };
                st.borrow().send(cid, S2C::SearchResults(hits));
            }
            C2S::Rename { session, name } => rename(st, cid, session, &name),
            C2S::Kill { session } => kill(st, cid, session),
            C2S::Respawn { session } => respawn(st, cid, session),
            C2S::Ping => {
                let mut s = st.borrow_mut();
                let ident = s.clients.get(&cid).and_then(|c| c.ident);
                let seq = match ident.and_then(|id| s.regs.get_mut(&id)) {
                    Some(r) => {
                        r.last_used = Instant::now();
                        r.last_input_seq
                    }
                    None => s.clients.get(&cid).map_or(0, |c| c.local_seq),
                };
                s.send(cid, S2C::Ack { last_input_seq: seq });
                s.send(cid, S2C::Pong);
            }
            C2S::Register => {
                let mut s = st.borrow_mut();
                if s.clients.get(&cid).is_some_and(|c| c.ident.is_some()) {
                    s.send(cid, S2C::Error("Register is only allowed on the control socket".into()));
                } else {
                    let reply = s.register();
                    s.send(cid, reply);
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn attach(
    st: &Shared,
    cid: u64,
    session: Option<SessionId>,
    cols: u16,
    rows: u16,
    term: &str,
    resume_from: Option<u64>,
    cwd: Option<&str>,
) {
    let mut s = st.borrow_mut();
    let sid = match session {
        Some(id) if s.sessions.contains_key(&id) => id,
        Some(_) => return s.send(cid, S2C::Error("no such session".into())),
        None => match s.create_session(cols, rows, term, cwd) {
            Ok(id) => {
                spawn_local(pty_task(st.clone(), id, s.sessions[&id].master.clone()));
                id
            }
            Err(e) => return s.send(cid, S2C::Error(format!("cannot start session: {e:#}"))),
        },
    };
    let c = s.clients.get_mut(&cid).unwrap();
    (c.session, c.cols, c.rows) = (Some(sid), cols, rows);

    let sess = s.sessions.get_mut(&sid).unwrap();
    sess.resize(cols, rows);
    sess.last_active = now_unix();
    sess.last_used = sess.last_active;
    s.send(cid, S2C::Attached { session: sid });

    let sess = &s.sessions[&sid];
    match resume_from.and_then(|o| sess.ring.since(o).map(|d| (o, d))) {
        Some((mut off, data)) => {
            for chunk in data.chunks(32 * 1024) {
                s.send(cid, S2C::Output { offset: off, data: chunk.to_vec() });
                off += chunk.len() as u64;
            }
        }
        None => s.send(cid, S2C::Redraw { offset: sess.ring.end(), data: sess.redraw() }),
    }
    s.broadcast_sessions();
}

fn rename(st: &Shared, cid: u64, sid: SessionId, name: &str) {
    let mut s = st.borrow_mut();
    let name: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
    let name = name.trim();
    let taken: Vec<String> = s.sessions.values().map(|x| x.name.clone()).collect();
    let Some(sess) = s.sessions.get_mut(&sid) else {
        return s.send(cid, S2C::Error("no such session".into()));
    };
    if name.is_empty() {
        let taken: Vec<&str> = taken.iter().map(String::as_str).collect();
        sess.name = naming::initial_name(&taken);
        sess.name_source = NameSource::Initial;
    } else {
        sess.name = name.to_string();
        sess.name_source = NameSource::User;
    }
    sess.save_meta();
    s.broadcast_sessions();
}

/// Close a session: an exited one goes away at once; otherwise SIGHUP every process group in it, SIGKILL them
/// two seconds later, and end the session then even if something (a process that escaped the session but kept
/// the pty open) would keep the terminal from closing.
fn kill(st: &Shared, cid: u64, sid: SessionId) {
    {
        let mut s = st.borrow_mut();
        let Some(sess) = s.sessions.get_mut(&sid) else {
            return s.send(cid, S2C::Error("no such session".into()));
        };
        if sess.exited {
            return s.end_session(sid);
        }
        sess.closing = true;
        sess.signal(Signal::SIGHUP);
    }
    let st = st.clone();
    spawn_local(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if let Some(sess) = st.borrow().sessions.get(&sid) {
            sess.signal(Signal::SIGKILL);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        st.borrow_mut().end_session(sid);
    });
}

/// New shell in an exited session.
fn respawn(st: &Shared, cid: u64, sid: SessionId) {
    let mut s = st.borrow_mut();
    let Some(sess) = s.sessions.get_mut(&sid) else {
        return s.send(cid, S2C::Error("no such session".into()));
    };
    if !sess.exited {
        return;
    }
    match sess.respawn() {
        Ok(()) => {
            spawn_local(pty_task(st.clone(), sid, sess.master.clone()));
            let redraw = S2C::Redraw { offset: sess.ring.end(), data: sess.redraw() };
            let ids: Vec<u64> = s.attached(sid).collect();
            for id in ids {
                s.send(id, redraw.clone());
            }
            s.broadcast_sessions();
        }
        Err(e) => s.send(cid, S2C::Error(format!("cannot start shell: {e:#}"))),
    }
}

/// Reads the PTY until the shell exits (EIO/EOF), then ends the session.
async fn pty_task(st: Shared, sid: SessionId, master: Master) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let Ok(mut guard) = master.readable().await else { break };
        match guard.try_io(|fd| nix::unistd::read(fd.get_ref(), &mut buf).map_err(std::io::Error::from)) {
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Ok(Ok(n)) => st.borrow_mut().on_output(sid, &buf[..n]),
            Err(_would_block) => {}
        }
    }
    st.borrow_mut().shell_exited(sid);
}

async fn pty_write(master: &AsyncFd<OwnedFd>, mut data: &[u8]) -> Result<()> {
    while !data.is_empty() {
        let mut guard = master.writable().await?;
        match guard.try_io(|fd| nix::unistd::write(fd.get_ref(), data).map_err(std::io::Error::from)) {
            Ok(Ok(n)) => data = &data[n..],
            Ok(Err(e)) => return Err(e.into()),
            Err(_would_block) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbox_is_bounded() {
        let ob = Outbox::new();
        ob.push(S2C::Pong);
        for i in 0..40u64 {
            ob.push(S2C::Output { offset: i, data: vec![0; 100 * 1024] });
        }
        assert!(ob.bytes.get() <= OUTBOX_LIMIT);
        assert!(ob.overflow.get());
        // Control messages survive the drop; queued output does not outlive the overflow.
        assert!(matches!(ob.pop(), Some(S2C::Pong)));
    }

    #[test]
    fn a_newer_session_list_replaces_an_unsent_one() {
        let ob = Outbox::new();
        let list = |n: usize| S2C::Sessions(Vec::with_capacity(n));
        ob.push(list(1));
        ob.push(S2C::Pong);
        ob.push(list(2));
        ob.push(list(3));
        assert!(matches!(ob.pop(), Some(S2C::Pong)));
        assert!(matches!(ob.pop(), Some(S2C::Sessions(_))));
        assert!(ob.pop().is_none(), "only the newest list is kept");
    }
}
