//! Key parsing, Emacs line editor, fuzzy filter and the session switcher overlay.

use std::time::{Duration, Instant};

use super::usage;
use crate::proto::{SearchHit, SessionId, SessionInfo};

/// Rows of the summary area, and the least terminal height (rows) that shows it.
const DETAIL_ROWS: usize = 3;
const DETAIL_MIN_ROWS: usize = 20;

/// Quiet time after the last keystroke before a full-text search is sent.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);

// ---------------------------------------------------------------- keys

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Alt(char),
    AltEnter,
    AltBackspace,
    Enter,
    Backspace,
    Delete,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
}

fn decode_char(b: &[u8]) -> (Option<char>, usize) {
    let n = match b[0] {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
    .min(b.len());
    (std::str::from_utf8(&b[..n]).ok().and_then(|s| s.chars().next()), n)
}

/// Turn a chunk of raw terminal input into keys. A lone ESC is the Escape key;
/// ESC followed by more bytes in the same chunk is Meta/an escape sequence.
pub fn parse_keys(b: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            0x1b if i + 1 >= b.len() => {
                keys.push(Key::Esc);
                i += 1;
            }
            0x1b => match b[i + 1] {
                b'[' | b'O' => {
                    let mut j = i + 2;
                    while j < b.len() && !(0x40..=0x7e).contains(&b[j]) {
                        j += 1;
                    }
                    if j >= b.len() {
                        break; // incomplete sequence
                    }
                    let key = match (b[j], &b[i + 2..j]) {
                        (b'A', _) => Some(Key::Up),
                        (b'B', _) => Some(Key::Down),
                        (b'C', _) => Some(Key::Right),
                        (b'D', _) => Some(Key::Left),
                        (b'H', _) => Some(Key::Home),
                        (b'F', _) => Some(Key::End),
                        (b'~', b"3") => Some(Key::Delete),
                        (b'~', b"5") => Some(Key::PageUp),
                        (b'~', b"6") => Some(Key::PageDown),
                        (b'~', b"1" | b"7") => Some(Key::Home),
                        (b'~', b"4" | b"8") => Some(Key::End),
                        _ => None,
                    };
                    keys.extend(key);
                    i = j + 1;
                }
                b'\r' | b'\n' => {
                    keys.push(Key::AltEnter);
                    i += 2;
                }
                0x7f => {
                    keys.push(Key::AltBackspace);
                    i += 2;
                }
                _ => {
                    let (ch, n) = decode_char(&b[i + 1..]);
                    keys.extend(ch.map(Key::Alt));
                    i += 1 + n;
                }
            },
            b'\r' | b'\n' => {
                keys.push(Key::Enter);
                i += 1;
            }
            0x7f => {
                keys.push(Key::Backspace);
                i += 1;
            }
            0x01..=0x1a => {
                keys.push(Key::Ctrl((c + 0x60) as char));
                i += 1;
            }
            // C-\ C-] C-^ C-_ (the prefix key is one of these by default).
            0x1c..=0x1f => {
                keys.push(Key::Ctrl((c + 0x40) as char));
                i += 1;
            }
            0x00 => i += 1,
            _ => {
                let (ch, n) = decode_char(&b[i..]);
                keys.extend(ch.map(Key::Char));
                i += n;
            }
        }
    }
    keys
}

// ---------------------------------------------------------------- line editor

#[derive(Default, Clone)]
pub struct LineEditor {
    chars: Vec<char>,
    pos: usize,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric()
}

impl LineEditor {
    pub fn with_text(s: &str) -> Self {
        let chars: Vec<char> = s.chars().collect();
        LineEditor { pos: chars.len(), chars }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn cursor(&self) -> usize {
        self.pos
    }

    /// The cursor is after the last character (where kill-line has nothing to kill).
    pub fn at_end(&self) -> bool {
        self.pos == self.chars.len()
    }

    fn word_left(&self) -> usize {
        let mut p = self.pos;
        while p > 0 && !is_word(self.chars[p - 1]) {
            p -= 1;
        }
        while p > 0 && is_word(self.chars[p - 1]) {
            p -= 1;
        }
        p
    }

    fn word_right(&self) -> usize {
        let mut p = self.pos;
        while p < self.chars.len() && !is_word(self.chars[p]) {
            p += 1;
        }
        while p < self.chars.len() && is_word(self.chars[p]) {
            p += 1;
        }
        p
    }

    fn delete_range(&mut self, a: usize, b: usize) {
        self.chars.drain(a..b);
        self.pos = a;
    }

    /// Apply an editing key. Returns true if the key was an editing key (it may not have changed anything).
    pub fn handle(&mut self, key: Key) -> bool {
        let len = self.chars.len();
        match key {
            Key::Char(c) => {
                self.chars.insert(self.pos, c);
                self.pos += 1;
            }
            Key::Ctrl('a') | Key::Home => self.pos = 0,
            Key::Ctrl('e') | Key::End => self.pos = len,
            Key::Ctrl('b') | Key::Left => self.pos = self.pos.saturating_sub(1),
            Key::Ctrl('f') | Key::Right => self.pos = (self.pos + 1).min(len),
            Key::Alt('b') => self.pos = self.word_left(),
            Key::Alt('f') => self.pos = self.word_right(),
            Key::Ctrl('d') | Key::Delete => {
                if self.pos < len {
                    self.delete_range(self.pos, self.pos + 1);
                }
            }
            Key::Ctrl('h') | Key::Backspace => {
                if self.pos > 0 {
                    self.delete_range(self.pos - 1, self.pos);
                }
            }
            Key::Ctrl('w') | Key::AltBackspace => self.delete_range(self.word_left(), self.pos),
            Key::Alt('d') => self.delete_range(self.pos, self.word_right()),
            Key::Ctrl('k') => self.delete_range(self.pos, len),
            Key::Ctrl('u') => self.delete_range(0, self.pos),
            _ => return false,
        }
        true
    }
}

// ---------------------------------------------------------------- fuzzy filter

/// Case-insensitive subsequence match; each whitespace-separated term of `query` must match.
/// Higher is better (consecutive runs and word starts score extra). Empty query matches everything.
pub fn fuzzy_score(query: &str, hay: &str) -> Option<i32> {
    let h: Vec<char> = hay.chars().flat_map(char::to_lowercase).collect();
    let mut total = 0;
    for term in query.split_whitespace() {
        let mut from = 0;
        let mut prev: Option<usize> = None;
        for qc in term.chars().flat_map(char::to_lowercase) {
            let at = (from..h.len()).find(|&k| h[k] == qc)?;
            total += 1;
            if prev.is_some_and(|p| p + 1 == at) {
                total += 5;
            }
            if at == 0 || !h[at - 1].is_alphanumeric() {
                total += 3;
            }
            prev = Some(at);
            from = at + 1;
        }
    }
    Some(total)
}

/// Fuzzy score of `query` against a session: each term matches the fuzzy haystack (id, name, command, cwd,
/// status, topic) or, as a plain case-insensitive substring, the summary (a subsequence match over 300 characters
/// of prose would match almost anything).
fn session_score(query: &str, s: &SessionInfo) -> Option<i32> {
    let hay = haystack(s);
    let summary = s.summary.to_lowercase();
    let mut total = 0;
    for term in query.split_whitespace() {
        total += fuzzy_score(term, &hay).or_else(|| summary.contains(&term.to_lowercase()).then_some(1))?;
    }
    Some(total)
}

fn haystack(s: &SessionInfo) -> String {
    let status = s.status.replace("{dur}", "").replace("{ago}", "");
    format!("#{} {} {} {} {} {}", s.id, s.name, if s.exited { "exited" } else { &s.fg_cmd }, s.cwd, status, s.recap)
}

// ---------------------------------------------------------------- switcher

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Cancel,
    Attach(SessionId),
    New,
    Rename(SessionId, String),
    Kill(SessionId),
    Detach,
    /// `<prefix> ?`
    Help,
}

enum Sub {
    Browse,
    Rename(SessionId, LineEditor),
    ConfirmKill(SessionId),
}

/// Entry order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sort {
    Recent,
    Cpu,
    Ram,
}

pub struct Switcher {
    sort: Sort,
    ascii: bool,
    sessions: Vec<SessionInfo>,
    current: Option<SessionId>,
    filter: LineEditor,
    sel: usize,
    top: usize,
    page: usize,
    sub: Sub,
    /// C-s: the query goes to the server (full-text search over transcripts) instead of filtering locally.
    search: bool,
    hits: Vec<SearchHit>,
    search_due: Option<Instant>,
    searched: String,
    waiting: bool,
    /// M-i: show the selected session's summary under the list (when the terminal is tall enough).
    detail: bool,
    /// The last naming failure reported by the server (empty = none), shown dim in the title bar.
    naming_error: bool,
    /// Why naming is paused (`battery`, ...; empty = not paused), shown dim in the title bar.
    naming_paused: String,
    /// Animation frame of the pending-topic spinner.
    spin: usize,
    /// Entry order, fixed when the switcher opens (alt-tab style): the current session, then the others by most
    /// recent use, exited ones last. Live updates change contents, never positions; new sessions go to the end.
    order: Vec<SessionId>,
    /// The client's prefix key as a Ctrl char (`C-]` -> `']'`), when it is not a letter (letters are line-editing keys).
    prefix: Option<char>,
    /// The previous key was the prefix (`C-] d` detaches) or `C-x` (`C-x C-c` detaches).
    chord: Option<char>,
}

/// The order a switcher opens with.
fn open_order(sessions: &[SessionInfo], current: Option<SessionId>) -> Vec<SessionId> {
    let mut v: Vec<&SessionInfo> = sessions.iter().collect();
    // Stable: ties keep the server's (id) order.
    v.sort_by_key(|s| (Some(s.id) != current, s.exited && Some(s.id) != current, std::cmp::Reverse(s.last_used_unix)));
    v.into_iter().map(|s| s.id).collect()
}

impl Switcher {
    /// Preselects the first entry: the current session, or, without one, the most recently used.
    pub fn new(sessions: Vec<SessionInfo>, current: Option<SessionId>) -> Self {
        let order = open_order(&sessions, current);
        let pos = |id: SessionId| order.iter().position(|&o| o == id).unwrap_or(usize::MAX);
        let sel = 0;
        let mut sessions = sessions;
        sessions.sort_by_key(|s| pos(s.id));
        Switcher {
            order,
            sort: Sort::Recent,
            ascii: usage::ascii_mode(),
            sessions,
            current,
            filter: LineEditor::default(),
            sel,
            top: 0,
            page: 5,
            sub: Sub::Browse,
            search: false,
            hits: Vec::new(),
            search_due: None,
            searched: String::new(),
            waiting: false,
            detail: true,
            naming_error: false,
            naming_paused: String::new(),
            spin: 0,
            prefix: None,
            chord: None,
        }
    }

    /// Tell the switcher the client's prefix byte so that `<prefix> d` detaches here too.
    pub fn set_prefix(&mut self, b: u8) {
        self.prefix = (0x1c..=0x1f).contains(&b).then(|| (b + 0x40) as char);
    }

    /// When the debounced search query should be sent (if one is pending).
    pub fn search_due(&self) -> Option<Instant> {
        self.search_due
    }

    /// The query to send now, if it is new.
    pub fn take_search(&mut self) -> Option<String> {
        self.search_due = None;
        let q = self.filter.text().trim().to_string();
        if q.is_empty() {
            self.hits.clear();
            self.searched.clear();
            self.waiting = false;
            return None;
        }
        if !self.search || q == self.searched {
            return None;
        }
        self.searched = q.clone();
        self.waiting = true;
        Some(q)
    }

    pub fn set_hits(&mut self, hits: Vec<SearchHit>) {
        if self.search {
            self.hits = hits;
            self.waiting = false;
            self.sel = 0;
            self.top = 0;
        }
    }

    fn count(&self) -> usize {
        if self.search { self.hits.len() } else { self.visible().len() }
    }

    /// Indices into `sessions`, best match first (id order when there is no filter).
    fn visible(&self) -> Vec<usize> {
        let q = self.filter.text();
        let mut v: Vec<(i32, usize)> = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(i, s)| session_score(&q, s).map(|sc| (sc, i)))
            .collect();
        // Position in the opening order: the recency tiebreak (new sessions come last).
        let rank = |i: usize| self.order.iter().position(|&o| o == self.sessions[i].id).unwrap_or(usize::MAX);
        if !q.trim().is_empty() {
            v.sort_by_key(|&(sc, i)| (-sc, rank(i)));
        } else {
            match self.sort {
                Sort::Recent => v.sort_by_key(|&(_, i)| rank(i)),
                Sort::Cpu => v.sort_by(|a, b| self.sessions[b.1].cpu_pct.total_cmp(&self.sessions[a.1].cpu_pct).then(rank(a.1).cmp(&rank(b.1)))),
                Sort::Ram => v.sort_by_key(|&(_, i)| (std::cmp::Reverse(self.sessions[i].rss_bytes), rank(i))),
            }
        }
        v.into_iter().map(|(_, i)| i).collect()
    }

    fn selected(&self) -> Option<&SessionInfo> {
        self.visible().get(self.sel).map(|&i| &self.sessions[i])
    }

    /// Close the selected session: an exited one at once, a live one after y/n.
    fn kill_selected(&mut self) -> Action {
        match self.selected().map(|s| (s.id, s.exited)) {
            Some((id, true)) => Action::Kill(id),
            Some((id, false)) => {
                self.sub = Sub::ConfirmKill(id);
                Action::None
            }
            None => Action::None,
        }
    }

    /// Rows for the summary area under the list: `DETAIL_ROWS` when it is on, the terminal is tall enough, the
    /// list keeps room for its entries and some session has a summary; else 0.
    fn detail_rows(&self, bh: usize, lh: usize) -> usize {
        let any = self.sessions.iter().any(|s| !s.summary.is_empty());
        let need = self.visible().len().clamp(1, 8);
        if self.detail && !self.search && any && bh >= DETAIL_MIN_ROWS && lh >= need + DETAIL_ROWS { DETAIL_ROWS } else { 0 }
    }

    pub fn set_naming_error(&mut self, e: &str) {
        self.naming_error = !e.is_empty();
    }

    pub fn set_naming_paused(&mut self, why: &str) {
        self.naming_paused = why.to_string();
    }

    /// Is some session waiting for the namer (the spinner runs)?
    pub fn any_pending(&self) -> bool {
        self.sessions.iter().any(|s| s.pending)
    }

    /// Advance the spinner one frame.
    pub fn tick_spinner(&mut self) {
        self.spin = self.spin.wrapping_add(1);
    }

    /// The spinner glyph for the current frame.
    fn spinner(&self) -> char {
        let frames: &[char] = if self.ascii { &['|', '/', '-', '\\'] } else { &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'] };
        frames[self.spin % frames.len()]
    }

    /// Replace the session list (server push), keeping the selection on the same session.
    pub fn update(&mut self, sessions: Vec<SessionInfo>, current: Option<SessionId>) {
        self.order.retain(|id| sessions.iter().any(|s| s.id == *id));
        for s in &sessions {
            if !self.order.contains(&s.id) {
                self.order.push(s.id);
            }
        }
        if self.search {
            self.sessions = sessions;
            self.current = current;
            return;
        }
        let id = self.selected().map(|s| s.id);
        self.sessions = sessions;
        self.current = current;
        let vis = self.visible();
        self.sel = id
            .and_then(|id| vis.iter().position(|&i| self.sessions[i].id == id))
            .unwrap_or(self.sel)
            .min(vis.len().saturating_sub(1));
    }

    pub fn handle(&mut self, key: Key) -> Action {
        let count = self.count();
        match &mut self.sub {
            Sub::ConfirmKill(id) => {
                let id = *id;
                self.sub = Sub::Browse;
                return if key == Key::Char('y') { Action::Kill(id) } else { Action::None };
            }
            Sub::Rename(id, ed) => {
                match key {
                    Key::Enter => {
                        let a = Action::Rename(*id, ed.text());
                        self.sub = Sub::Browse;
                        return a;
                    }
                    Key::Ctrl('g') | Key::Esc => self.sub = Sub::Browse,
                    k => {
                        ed.handle(k);
                    }
                }
                return Action::None;
            }
            Sub::Browse => {}
        }
        let last = count.saturating_sub(1);
        // Detach chords: `<prefix> d` and `C-x C-c`.
        let pending = self.chord.take();
        match (pending, key) {
            (Some(']' | '\\' | '^' | '_'), Key::Char('d')) | (Some('x'), Key::Ctrl('c')) => return Action::Detach,
            (Some(']' | '\\' | '^' | '_'), Key::Char('?')) => return Action::Help,
            // The chord's first key never acts on its own.
            (None, Key::Ctrl('x')) => {
                self.chord = Some('x');
                return Action::None;
            }
            (None, Key::Ctrl(c)) if Some(c) == self.prefix => {
                self.chord = Some(c);
                return Action::None;
            }
            (Some(_), Key::Ctrl(c)) if Some(c) == self.prefix => {
                self.chord = Some(c);
                return Action::None;
            }
            _ => {}
        }
        match key {
            Key::Ctrl('g') | Key::Esc => return Action::Cancel,
            Key::Enter => {
                if self.search {
                    if let Some(h) = self.hits.get(self.sel) {
                        return Action::Attach(h.session);
                    }
                } else if let Some(s) = self.selected() {
                    return Action::Attach(s.id);
                }
            }
            Key::Ctrl('s') => {
                self.search = !self.search;
                self.sel = 0;
                self.top = 0;
                self.hits.clear();
                self.searched.clear();
                self.waiting = false;
                self.search_due = (self.search && !self.filter.text().trim().is_empty()).then(Instant::now);
            }
            Key::AltEnter | Key::Ctrl('o') => return Action::New,
            Key::Alt('s') if !self.search => {
                let id = self.selected().map(|s| s.id);
                self.sort = match self.sort {
                    Sort::Recent => Sort::Cpu,
                    Sort::Cpu => Sort::Ram,
                    Sort::Ram => Sort::Recent,
                };
                if let Some(id) = id {
                    self.sel = self.visible().iter().position(|&i| self.sessions[i].id == id).unwrap_or(0);
                }
                self.top = 0;
            }
            Key::Alt('i') => self.detail = !self.detail,
            Key::Alt('r') | Key::Alt('k') if self.search => {}
            Key::Alt('r') => {
                if let Some(s) = self.selected() {
                    self.sub = Sub::Rename(s.id, LineEditor::with_text(&s.name));
                }
            }
            // C-k kills the selected session when the cursor is at the end of the filter text (where kill-line
            // would do nothing); anywhere else it is kill-line. In search mode it always edits the query.
            Key::Alt('k') => return self.kill_selected(),
            Key::Ctrl('k') if !self.search && self.filter.at_end() => return self.kill_selected(),
            Key::Ctrl('n') | Key::Down => self.sel = (self.sel + 1).min(last),
            Key::Ctrl('p') | Key::Up => self.sel = self.sel.saturating_sub(1),
            Key::Ctrl('v') | Key::PageDown => self.sel = (self.sel + self.page).min(last),
            Key::Alt('v') | Key::PageUp => self.sel = self.sel.saturating_sub(self.page),
            Key::Alt('<') => self.sel = 0,
            Key::Alt('>') => self.sel = last,
            k => {
                if self.filter.handle(k) {
                    self.sel = 0;
                    self.top = 0;
                    if self.search {
                        self.search_due = Some(Instant::now() + SEARCH_DEBOUNCE);
                    }
                }
            }
        }
        Action::None
    }

    /// Escape sequences that draw the overlay on a `cols` x `rows` terminal.
    pub fn render(&mut self, cols: u16, rows: u16, now: u64) -> String {
        let (cols, rows) = (cols as usize, rows as usize);
        let (bw, bh) = if rows < 50 { (cols, rows) } else { (cols.min(100), rows.min(30)) };
        let (bx, by) = ((cols - bw) / 2, (rows - bh) / 2);
        let iw = bw.saturating_sub(2);
        let ih = bh.saturating_sub(2);
        let dh = self.detail_rows(bh, ih.saturating_sub(2).max(1));
        let lh = (ih.saturating_sub(2 + dh)).max(1);
        self.page = lh.saturating_sub(1).max(1);

        let vis = if self.search { Vec::new() } else { self.visible() };
        let count = self.count();
        self.sel = self.sel.min(count.saturating_sub(1));

        // Row styles: 0 plain, 1 selected, 2 dim topic, 3 selected dim topic, 4 status, 5 selected status.
        let mut lines: Vec<(String, u8)> = Vec::new();
        if self.search {
            self.top = self.top.min(self.sel);
            if self.sel >= self.top + lh {
                self.top = self.sel + 1 - lh;
            }
            let q = self.filter.text();
            let msg = if q.trim().is_empty() {
                Some("type to search session transcripts")
            } else if self.hits.is_empty() && self.search_due.is_none() && !self.waiting {
                Some("no matches")
            } else {
                None
            };
            for r in 0..lh {
                let line = match (self.hits.get(self.top + r), msg) {
                    (Some(h), _) => {
                        let name = self.sessions.iter().find(|s| s.id == h.session).map_or("?", |s| s.name.as_str());
                        let sel = self.top + r == self.sel;
                        (fit(&format!(" #{:<3} {}: {}", h.session, name, h.line), iw), sel as u8)
                    }
                    (None, Some(m)) if r == 0 => (fit(&format!(" {m}"), iw), 2),
                    _ => (fit("", iw), 0),
                };
                lines.push(line);
            }
        } else {
            let has_status = |pos: usize| {
                let e = &self.sessions[vis[pos]];
                !e.status.is_empty() && !e.exited
            };
            let has_topic = |pos: usize| {
                let e = &self.sessions[vis[pos]];
                !e.recap.is_empty() || e.pending
            };
            let (st, tp) = (|p: usize| has_status(p) as usize, |p: usize| has_topic(p) as usize);
            let lay = usage::layout(bw);
            let scale = usage::Scale::of(&self.sessions);
            let top_rss = self.sessions.iter().map(|s| s.rss_bytes).max().unwrap_or(0);
            // Narrow terminals have no room for numbers: the selected entry gets a detail line instead.
            let dt = |p: usize| (lay == usage::Layout::Narrow && p == self.sel) as usize;
            // Status and topic under every entry if they fit, else only the status, else only under the selected entry.
            let tier = if (0..vis.len()).map(|p| 1 + st(p) + tp(p) + dt(p)).sum::<usize>() <= lh {
                2
            } else if (0..vis.len()).map(|p| 1 + st(p) + dt(p)).sum::<usize>() <= lh {
                1
            } else {
                0
            };
            // One blank line between entries while everything (status and topic lines too) still fits.
            let n = vis.len();
            let gap = tier == 2 && n > 1 && (0..n).map(|p| 1 + st(p) + tp(p) + dt(p)).sum::<usize>() + n - 1 <= lh;
            let sel = self.sel;
            let height = |pos: usize| match tier {
                2 => 1 + st(pos) + tp(pos) + dt(pos),
                1 => 1 + st(pos) + dt(pos),
                _ if pos == sel => (1 + st(pos) + tp(pos) + dt(pos)).min(lh),
                _ => 1,
            };
            self.top = self.top.min(sel);
            while self.top < sel && (self.top..=sel).map(height).sum::<usize>() > lh {
                self.top += 1;
            }
            let name_w = vis
                .iter()
                .map(|&i| self.sessions[i].name.chars().count())
                .max()
                .unwrap_or(0)
                .clamp(6, 24)
                .min(iw.saturating_sub(12));
            let mut pos = self.top;
            while lines.len() < lh {
                let Some(&i) = vis.get(pos) else {
                    lines.push((fit("", iw), 0));
                    continue;
                };
                let s = &self.sessions[i];
                let selected = pos == sel;
                let mark = if Some(s.id) == self.current { '*' } else { ' ' };
                let head = format!(" {mark}#{:<3} {} [{}] {} ", s.id, fit(&s.name, name_w), s.clients, fit(if s.exited { "exited" } else { &s.fg_cmd }, 8));
                let age = age(now, s.last_active_unix);
                let (res, res_w) = usage::cell(s, &scale, s.rss_bytes == top_rss && top_rss > 0, lay, self.ascii);
                let fixed = head.chars().count() + age.len() + 1;
                // Drop the usage cell when even a short cwd would not fit next to it.
                let show = iw >= fixed + res_w + 1 + if lay == usage::Layout::Narrow { 0 } else { 8 };
                let w = if show { res_w + 1 } else { 0 };
                let cwd = fit_left(&s.cwd, iw.saturating_sub(fixed + w));
                if show {
                    lines.push((format!("{head}{cwd} {res} {age}"), selected as u8));
                } else {
                    lines.push((fit(&format!("{head}{cwd} {age}"), iw), selected as u8));
                }
                let mut extras: Vec<(String, u8)> = Vec::new();
                if dt(pos) == 1 {
                    extras.push((fit(&format!("       {}", usage::detail(s)), iw), 2 + selected as u8));
                }
                if has_status(pos) {
                    let text = crate::proto::expand_status(&s.status, s.status_since, now);
                    extras.push((fit(&format!("       {text}"), iw), 4 + selected as u8));
                }
                if has_topic(pos) {
                    let t = if s.pending { format!("{} {}", s.recap, self.spinner()).trim().to_string() } else { s.recap.clone() };
                    extras.push((fit(&format!("       {t}"), iw), 2 + selected as u8));
                }
                // Tier 1 shows no topic: it is the last extra, and `height` leaves no room for it.
                for e in extras.into_iter().take(height(pos) - 1) {
                    if lines.len() < lh {
                        lines.push(e);
                    }
                }
                pos += 1;
                if gap && pos < n && lines.len() < lh {
                    lines.push((fit("", iw), 0));
                }
            }
        }

        // Summary of the selected session, word-wrapped and dim.
        let detail: Vec<String> = if dh == 0 {
            Vec::new()
        } else {
            let text = vis.get(self.sel).map_or("", |&i| self.sessions[i].summary.as_str());
            wrap(text, iw.saturating_sub(2), dh).into_iter().map(|l| fit(&format!(" {l}"), iw)).collect()
        };

        // Prompt line, with the cursor position inside it.
        let (label, text, cursor) = match &self.sub {
            Sub::Rename(_, ed) => ("rename: ", ed.text(), ed.cursor()),
            _ if self.search => ("search: ", self.filter.text(), self.filter.cursor()),
            _ => ("> ", self.filter.text(), self.filter.cursor()),
        };
        let avail = iw.saturating_sub(label.len() + 1).max(1);
        let shown: String = text.chars().take(avail).collect();
        let prompt = fit(&format!("{label}{shown}"), iw);
        let cursor_col = bx + 1 + label.len() + cursor.min(avail);

        let hint = match &self.sub {
            Sub::ConfirmKill(id) => {
                let name = self.sessions.iter().find(|s| s.id == *id).map_or("", |s| s.name.as_str());
                format!("kill #{id} {name}? (y/n)")
            }
            _ if self.search => [
                "RET attach  C-s back to filter  C-g cancel",
                "RET go  C-s filter  C-g quit",
                "RET C-s C-g",
                "RET C-g",
            ]
            .into_iter()
            .find(|h| h.len() <= iw)
            .unwrap_or("")
            .to_string(),
            _ => {
                let so = match self.sort {
                    Sort::Recent => "M-s sort".to_string(),
                    Sort::Cpu => "M-s sort:cpu".to_string(),
                    Sort::Ram => "M-s sort:ram".to_string(),
                };
                // M-i is only advertised where the summary area could be shown (tall enough, some summary exists).
                let info = self.sessions.iter().any(|s| !s.summary.is_empty()) && bh >= DETAIL_MIN_ROWS;
                let i = if info { "M-i info  " } else { "" };
                [
                    format!("RET attach  C-s search  C-o new  M-r rename  C-k kill  {i}{so}  C-] d detach  C-g cancel"),
                    format!("RET attach  C-s search  C-o new  M-r rename  C-k kill  {i}{so}  C-g cancel"),
                    format!("RET go  C-s find  C-o new  M-r ren  C-k kill  {i}{so}  C-] d detach  C-g quit"),
                    format!("RET go  C-s find  C-o new  M-r ren  C-k kill  {i}{so}  C-g quit"),
                    format!("RET attach  C-s search  C-o new  M-r rename  C-k kill  {so}  C-g cancel"),
                    format!("RET go  C-s find  C-o new  M-r ren  C-k kill  {so}  C-g quit"),
                    format!("RET C-s C-o M-r C-k {so}  C-g"),
                    "RET C-s C-o M-r C-k M-s C-g".to_string(),
                    "RET C-g".to_string(),
                ]
                .into_iter()
                .find(|h| h.chars().count() <= iw)
                .unwrap_or_default()
            }
        };

        let mut out = String::from("\x1b[?25l\x1b[0m");
        let mut put = |row: usize, text: &str| out.push_str(&format!("\x1b[{};{}H{}", by + row + 1, bx + 1, text));
        let total = usage::total(&self.sessions);
        let title = if self.search {
            format!("─ nt search ({} hits) ", count)
        } else if self.sessions.iter().any(|s| s.rss_bytes > 0) && total.chars().count() + 6 <= iw {
            format!("─ nt: {total} ")
        } else {
            format!("─ nt sessions ({count}) ")
        };
        const NAMING_ERR: &str = " naming: error, see server.log ";
        let tl = title.chars().count();
        if self.naming_error && !self.search && tl + NAMING_ERR.chars().count() + 2 <= iw {
            let fill = iw - tl - NAMING_ERR.chars().count() - 1;
            put(0, &format!("┌{title}{}\x1b[2m{NAMING_ERR}\x1b[0m─┐", "─".repeat(fill)));
        } else if !self.naming_paused.is_empty() && !self.search && tl + self.naming_paused.chars().count() + 17 <= iw {
            let note = format!(" naming paused: {} ", self.naming_paused);
            let fill = iw - tl - note.chars().count() - 1;
            put(0, &format!("┌{title}{}\x1b[2m{note}\x1b[0m─┐", "─".repeat(fill)));
        } else {
            put(0, &format!("┌{}┐", fit_fill(&title, iw, '─')));
        }
        for row in 1..bh.saturating_sub(1) {
            let body = match row {
                1 => prompt.clone(),
                r if r <= lh + 1 => {
                    let (t, sel) = &lines[r - 2];
                    let gutter = |t: &str| {
                        let rest: String = t.chars().skip(1).collect();
                        let g = if self.ascii { ">" } else { "▌" };
                        (format!("\x1b[36m{g}\x1b[0m"), rest)
                    };
                    match sel {
                        0 => t.clone(),
                        1 => {
                            // Selected entry: accent gutter and a bold `#id name`, everything else as it is.
                            let (g, rest) = gutter(t);
                            let cut = rest.find(" [").unwrap_or(rest.len());
                            format!("{g}\x1b[1m{}\x1b[22m{}", &rest[..cut], &rest[cut..])
                        }
                        2 => format!("\x1b[2m{t}\x1b[0m"),
                        3 => {
                            let (g, rest) = gutter(t);
                            format!("{g}{rest}")
                        }
                        4 => status_row(t, false),
                        _ => {
                            // The body starts with the plain blank of the gutter column: swap it for the marker.
                            let (g, _) = gutter(t);
                            format!("{g}{}", status_row(t, true).chars().skip(1).collect::<String>())
                        }
                    }
                }
                r if r > lh + 1 && r <= lh + 1 + dh => format!("\x1b[2m{}\x1b[0m", detail.get(r - lh - 2).cloned().unwrap_or_else(|| fit("", iw))),
                r if r == bh - 2 => format!("\x1b[2m{}\x1b[0m", fit(&hint, iw)),
                _ => fit("", iw),
            };
            put(row, &format!("│{body}│"));
        }
        put(bh - 1, &format!("└{}┘", "─".repeat(iw)));
        out.push_str(&format!("\x1b[{};{}H\x1b[?25h", by + 2, cursor_col + 1));
        out
    }
}

// ---------------------------------------------------------------- text helpers

/// A status row (6 columns of indent, then the status): the leading symbol is coloured
/// (green check, red cross, yellow running/editing); other statuses are dim.
fn status_row(t: &str, selected: bool) -> String {
    let (pre, rest) = t.split_at(t.char_indices().nth(7).map_or(t.len(), |(i, _)| i));
    let mut it = rest.chars();
    let body = match it.next() {
        Some(c @ ('✓' | '✗' | '⏵' | '✎')) => {
            let color = match c {
                '✓' => "32",
                '✗' => "31",
                _ => "33",
            };
            format!("{pre}\x1b[{color}m{c}\x1b[39m{}", it.as_str())
        }
        // Plain statuses: dim, unless the entry is selected.
        _ if selected => t.to_string(),
        _ => format!("\x1b[2m{t}\x1b[22m"),
    };
    format!("{body}\x1b[0m")
}

fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// Truncate or pad with spaces to exactly `w` columns (counting chars).
pub fn fit(s: &str, w: usize) -> String {
    fit_fill(s, w, ' ')
}

fn fit_fill(s: &str, w: usize, fill: char) -> String {
    let mut out: String = clean(s).chars().take(w).collect();
    let n = out.chars().count();
    out.extend(std::iter::repeat_n(fill, w - n));
    out
}

/// Greedy word wrap to at most `max` lines of `w` columns; text that does not fit ends in an ellipsis.
pub fn wrap(text: &str, w: usize, max: usize) -> Vec<String> {
    let w = w.max(4);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in clean(text).split_whitespace() {
        // Break words longer than a line.
        let mut word = word.to_string();
        while word.chars().count() > w {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let head: String = word.chars().take(w).collect();
            word = word.chars().skip(w).collect();
            lines.push(head);
        }
        if cur.is_empty() {
            cur = word;
        } else if cur.chars().count() + 1 + word.chars().count() <= w {
            cur.push(' ');
            cur.push_str(&word);
        } else {
            lines.push(std::mem::replace(&mut cur, word));
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.len() > max {
        lines.truncate(max);
        let last = lines.pop().unwrap_or_default();
        let cut: String = last.chars().take(w - 1).collect();
        lines.push(format!("{}…", cut.trim_end()));
    }
    lines
}

/// Like `fit`, but keeps the end of the string (for paths), marking the cut with an ellipsis.
fn fit_left(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        return fit(s, w);
    }
    if w == 0 {
        return String::new();
    }
    let tail: String = s.chars().skip(n - (w - 1)).collect();
    format!("…{}", clean(&tail))
}

pub fn age(now: u64, then: u64) -> String {
    let d = now.saturating_sub(then);
    match d {
        0..=59 => format!("{d}s"),
        60..=3599 => format!("{}m", d / 60),
        3600..=86399 => format!("{}h", d / 3600),
        _ => format!("{}d", d / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::NameSource;

    fn ed(s: &str) -> LineEditor {
        LineEditor::with_text(s)
    }

    fn keys(e: &mut LineEditor, ks: &[Key]) {
        for &k in ks {
            e.handle(k);
        }
    }

    #[test]
    fn editor_basic_motion_and_delete() {
        let mut e = ed("hello world");
        keys(&mut e, &[Key::Ctrl('a'), Key::Ctrl('f'), Key::Ctrl('d')]);
        assert_eq!(e.text(), "hllo world");
        keys(&mut e, &[Key::Ctrl('e'), Key::Ctrl('h'), Key::Backspace]);
        assert_eq!(e.text(), "hllo wor");
        keys(&mut e, &[Key::Ctrl('b'), Key::Ctrl('b'), Key::Char('X')]);
        assert_eq!(e.text(), "hllo wXor");
    }

    #[test]
    fn editor_words_and_kills() {
        let mut e = ed("foo bar baz");
        keys(&mut e, &[Key::Alt('b')]);
        assert_eq!(e.cursor(), 8);
        keys(&mut e, &[Key::Alt('b'), Key::Alt('f')]);
        assert_eq!(e.cursor(), 7);
        keys(&mut e, &[Key::Ctrl('w')]);
        assert_eq!(e.text(), "foo  baz");
        keys(&mut e, &[Key::Alt('d')]);
        assert_eq!(e.text(), "foo ");
        keys(&mut e, &[Key::Ctrl('a'), Key::Ctrl('k')]);
        assert_eq!(e.text(), "");
        let mut e = ed("abc def");
        keys(&mut e, &[Key::Ctrl('b'), Key::Ctrl('u')]);
        assert_eq!(e.text(), "f");
        assert_eq!(e.cursor(), 0);
    }

    #[test]
    fn editor_unicode() {
        let mut e = ed("héllo");
        keys(&mut e, &[Key::Ctrl('a'), Key::Ctrl('f'), Key::Ctrl('d')]);
        assert_eq!(e.text(), "hllo");
    }

    #[test]
    fn fuzzy_matching() {
        assert!(fuzzy_score("", "anything").is_some());
        assert!(fuzzy_score("nv", "nt:vim").is_some());
        assert!(fuzzy_score("NV", "nt:vim").is_some());
        assert!(fuzzy_score("vn", "nt:vim").is_none());
        assert!(fuzzy_score("zzz", "nt:vim").is_none());
        // consecutive beats scattered
        assert!(fuzzy_score("vim", "xx vim") > fuzzy_score("vim", "v-i-m x"));
        // several terms, order independent
        assert!(fuzzy_score("home vim", "#3 calm-otter vim /home/a").is_some());
    }

    #[test]
    fn key_parsing() {
        use Key::*;
        assert_eq!(parse_keys(b"a\x01\r"), vec![Char('a'), Ctrl('a'), Enter]);
        assert_eq!(parse_keys(b"\x1b"), vec![Esc]);
        assert_eq!(parse_keys(b"\x1b[A\x1bOB\x1b[3~"), vec![Up, Down, Delete]);
        assert_eq!(parse_keys(b"\x1bb\x1bf\x1b\r\x1b\x7f"), vec![Alt('b'), Alt('f'), AltEnter, AltBackspace]);
        assert_eq!(parse_keys("é".as_bytes()), vec![Char('é')]);
        assert_eq!(parse_keys(b"\x7f\x08"), vec![Backspace, Ctrl('h')]);
    }

    fn info(id: u32, name: &str, clients: u32, active: u64) -> SessionInfo {
        SessionInfo {
            id,
            name: name.into(),
            name_source: NameSource::Initial,
            created_unix: 0,
            last_active_unix: active,
            last_used_unix: active,
            clients,
            cols: 80,
            rows: 24,
            cwd: "/home/x".into(),
            fg_cmd: "bash".into(),
            recap: String::new(),
            summary: String::new(),
            exited: false,
            status: String::new(),
            status_since: 0,
            cpu_pct: 0.0,
            rss_bytes: 0,
            pending: false,
        }
    }

    fn ids(sw: &Switcher) -> Vec<SessionId> {
        sw.visible().into_iter().map(|i| sw.sessions[i].id).collect()
    }

    #[test]
    fn order_current_first_then_recent_use_exited_last() {
        let mut ex = info(5, "gone", 0, 99);
        ex.exited = true;
        let list = vec![info(1, "a-a", 0, 10), info(2, "b-b", 0, 30), info(3, "c-c", 0, 20), info(4, "d-d", 0, 5), ex];
        // current = 3: it leads, then by last use (2, 1, 4), the exited one last; the current session is preselected
        let sw = Switcher::new(list.clone(), Some(3));
        assert_eq!(ids(&sw), vec![3, 2, 1, 4, 5]);
        assert_eq!(sw.selected().unwrap().id, 3);
        // without a current session: the most recently used one first and preselected
        let sw = Switcher::new(list, None);
        assert_eq!(ids(&sw), vec![2, 3, 1, 4, 5]);
        assert_eq!(sw.selected().unwrap().id, 2);
        let sw = Switcher::new(vec![info(7, "x-y", 0, 1)], Some(7));
        assert_eq!(sw.selected().unwrap().id, 7);
    }

    #[test]
    fn order_is_stable_while_open() {
        let list = vec![info(1, "a-a", 0, 10), info(2, "b-b", 0, 30), info(3, "c-c", 0, 20)];
        let mut sw = Switcher::new(list.clone(), Some(1));
        assert_eq!(ids(&sw), vec![1, 2, 3]);
        // session 3 becomes the most recently used, a new session 4 appears, 2 disappears
        let mut upd = vec![info(1, "a-a", 0, 10), info(3, "c-c", 0, 500), info(4, "n-n", 0, 900)];
        upd[1].pending = true;
        sw.update(upd, Some(1));
        assert_eq!(ids(&sw), vec![1, 3, 4]);
        // contents changed, positions did not
        assert!(sw.sessions.iter().any(|s| s.id == 3 && s.pending));
        // the selection follows the session, not the index
        let mut sw = Switcher::new(list, Some(1));
        sw.handle(Key::Down); // on 3
        let before = sw.selected().unwrap().id;
        sw.update(vec![info(1, "a-a", 0, 10), info(3, "c-c", 0, 20), info(2, "b-b", 0, 99)], Some(1));
        assert_eq!(sw.selected().unwrap().id, before);
        // filtering ranks by score, recency order breaks ties
        let mut sw = Switcher::new(vec![info(1, "x-ab", 0, 10), info(2, "x-cd", 0, 30), info(3, "x-ef", 0, 20)], None);
        for c in "x-".chars() {
            sw.handle(Key::Char(c));
        }
        assert_eq!(ids(&sw), vec![2, 3, 1]);
    }

    #[test]
    fn selected_entry_has_gutter_and_no_reverse_video() {
        let mut a = with_recap(info(1, "calm-otter", 1, 50), "fixing the build");
        a.status = "✓ cargo build · {ago}".into();
        let b = with_recap(info(2, "brave-lynx", 0, 40), "reading docs");
        for (cols, rows) in [(80u16, 24u16), (40, 10)] {
            let mut sw = Switcher::new(vec![a.clone(), b.clone()], Some(1));
            let out = sw.render(cols, rows, 100);
            assert!(!out.contains("\x1b[7m") && !out.contains("\x1b[7;2m"), "{cols}x{rows}: reverse video left");
            assert!(out.contains("\x1b[36m▌"), "{cols}x{rows}: gutter missing");
            assert!(out.contains("\x1b[1m"), "{cols}x{rows}: bold name missing");
            // every row is still exactly as wide as the box
            for r in 1..=rows as usize {
                let t = text_at(&out, r);
                if !t.is_empty() {
                    assert_eq!(t.chars().count(), cols as usize, "{cols}x{rows} row {r}: {t:?}");
                }
            }
        }
        // the selected entry's topic is normal text, the other entry's topic is dim
        let mut sw = Switcher::new(vec![a.clone(), b.clone()], Some(1));
        let out = sw.render(80, 24, 100);
        assert!(out.contains("\x1b[2m") && out.contains("reading docs"));
        sw.ascii = true;
        assert!(sw.render(80, 24, 100).contains("\x1b[36m>"));
    }

    #[test]
    fn pending_spinner_after_topic() {
        let mut a = with_recap(info(1, "a-b", 0, 1), "fixing the build");
        let mut sw = Switcher::new(vec![a.clone()], None);
        sw.ascii = false;
        let topic_row = |sw: &mut Switcher| (1..=24).map(|r| text_at(&sw.render(80, 24, 100), r)).find(|l| l.contains("fixing the build")).unwrap().trim_matches(|c: char| c == '│' || c == ' ').to_string();
        // nothing pending: no spinner, no timer need
        assert!(!sw.any_pending());
        let idle = topic_row(&mut sw);
        assert!(idle.ends_with("fixing the build"), "{idle}");
        sw.tick_spinner();
        assert_eq!(topic_row(&mut sw), idle);
        a.pending = true;
        sw.update(vec![a], None);
        assert!(sw.any_pending());
        let f0 = topic_row(&mut sw);
        assert!(f0.contains("fixing the build ") && f0.contains(|c| ('\u{2800}'..='\u{28ff}').contains(&c)), "{f0}");
        sw.tick_spinner();
        let f1 = topic_row(&mut sw);
        assert_ne!(f0, f1);
        // ten frames wrap around
        for _ in 0..10 {
            sw.tick_spinner();
        }
        assert_eq!(topic_row(&mut sw), f1);
        // ASCII mode cycles | / - \
        sw.ascii = true;
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(topic_row(&mut sw).chars().last().unwrap());
            sw.tick_spinner();
        }
        seen.sort();
        assert_eq!(seen, vec!['-', '/', '\\', '|']);
    }

    #[test]
    fn naming_paused_in_title() {
        let mut sw = Switcher::new(vec![info(1, "a-b", 0, 1)], None);
        sw.set_naming_paused("battery");
        let top = text_at(&sw.render(80, 24, 100), 1);
        assert!(top.contains("naming paused: battery") && top.chars().count() == 80, "{top}");
        sw.set_naming_paused("");
        assert!(!text_at(&sw.render(80, 24, 100), 1).contains("paused"));
    }

    #[test]
    fn naming_error_in_title() {
        let mut sw = Switcher::new(vec![info(1, "a-b", 0, 1)], None);
        assert!(!text_at(&sw.render(80, 24, 100), 1).contains("naming"));
        sw.set_naming_error("spawn failed");
        let top = text_at(&sw.render(80, 24, 100), 1);
        assert!(top.contains("naming: error, see server.log") && top.chars().count() == 80, "{top}");
        assert!(!text_at(&sw.render(40, 10, 100), 1).contains("naming: error, see") || text_at(&sw.render(40, 10, 100), 1).chars().count() == 40);
    }

    #[test]
    fn switcher_flow() {
        let list = vec![info(1, "calm-otter", 1, 50), info(2, "brave-lynx", 0, 10), info(3, "icy-heron", 0, 40)];
        let mut sw = Switcher::new(list, Some(1));
        // the current session (id 1) is preselected
        assert_eq!(sw.handle(Key::Enter), Action::Attach(1));
        // filtering
        for c in "lynx".chars() {
            sw.handle(Key::Char(c));
        }
        assert_eq!(sw.handle(Key::Enter), Action::Attach(2));
        sw.handle(Key::Ctrl('u'));
        // recent order: 1 (50), 3 (40), 2 (10)
        assert_eq!(sw.handle(Key::Ctrl('n')), Action::None);
        assert_eq!(sw.handle(Key::Enter), Action::Attach(3));
        sw.handle(Key::Ctrl('n'));
        assert_eq!(sw.handle(Key::Enter), Action::Attach(2));
        // rename
        sw.handle(Key::Alt('r'));
        sw.handle(Key::Ctrl('u'));
        sw.handle(Key::Char('z'));
        assert_eq!(sw.handle(Key::Enter), Action::Rename(2, "z".into()));
        // kill needs confirmation
        sw.handle(Key::Alt('k'));
        assert_eq!(sw.handle(Key::Char('n')), Action::None);
        sw.handle(Key::Alt('k'));
        assert_eq!(sw.handle(Key::Char('y')), Action::Kill(2));
        assert_eq!(sw.handle(Key::Ctrl('o')), Action::New);
        assert_eq!(sw.handle(Key::Ctrl('g')), Action::Cancel);
    }

    #[test]
    fn render_small_and_large() {
        let list = vec![info(1, "calm-otter", 1, 50), info(2, "brave-lynx", 0, 10)];
        let mut sw = Switcher::new(list, None);
        let out = sw.render(40, 10, 100);
        assert!(out.contains("calm-otter") && out.contains("brave-lynx"));
        assert!(sw.render(200, 60, 100).contains("nt sessions"));
    }

    fn with_recap(mut i: SessionInfo, r: &str) -> SessionInfo {
        i.recap = r.into();
        i
    }

    /// (row, visible text) of every `put` in a render, escape sequences removed.
    fn rows_of(out: &str) -> Vec<(usize, String)> {
        let mut v = Vec::new();
        for part in out.split("\x1b[").skip(1) {
            let Some((pos, rest)) = part.split_once('H') else { continue };
            let Some((r, _)) = pos.split_once(';') else { continue };
            let Ok(r) = r.parse() else { continue };
            let mut text = String::new();
            let mut it = rest.chars();
            while let Some(c) = it.next() {
                if c == '\x1b' {
                    for c in it.by_ref() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else {
                    text.push(c);
                }
            }
            v.push((r, text));
        }
        v
    }

    #[test]
    fn recap_line_and_filter() {
        let list = vec![
            with_recap(info(1, "calm-otter", 0, 50), "debugging tcp reconnect in nt"),
            with_recap(info(2, "brave-lynx", 0, 10), "editing nginx config"),
        ];
        let mut sw = Switcher::new(list, None);
        let out = sw.render(80, 24, 100);
        assert!(out.contains("debugging tcp reconnect") && out.contains("\x1b[2m"));
        // the filter matches the recap
        for c in "nginx".chars() {
            sw.handle(Key::Char(c));
        }
        assert_eq!(sw.handle(Key::Enter), Action::Attach(2));
    }

    #[test]
    fn fits_40x10_with_recaps() {
        let list: Vec<SessionInfo> =
            (1..=9).map(|i| with_recap(info(i, "calm-otter", 0, i as u64), "a fairly long recap line that cannot possibly fit in forty columns")).collect();
        let mut sw = Switcher::new(list, None);
        for step in 0..12 {
            let out = sw.render(40, 10, 100);
            let rows = rows_of(&out);
            assert!(rows.iter().all(|(r, t)| *r <= 10 && t.chars().count() <= 40), "step {step}: {rows:?}");
            // only the selected entry shows its recap when there is no room for all
            assert_eq!(out.matches("a fairly long").count(), 1, "step {step}");
            sw.handle(Key::Ctrl('n'));
        }
    }

    /// Screen row of each of the entries `#1 ..= #n` (escape sequences removed first, so styled rows count too).
    fn entry_rows(out: &str, n: u32) -> Vec<usize> {
        let mut rows: Vec<(usize, String)> = Vec::new();
        for part in out.split("\x1b[").skip(1) {
            let end = part.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(0);
            let (seq, text) = (&part[..end], &part[end + 1..]);
            if part[end..].starts_with('H') {
                rows.push((seq.split(';').next().and_then(|r| r.parse().ok()).unwrap_or(0), text.to_string()));
            } else if let Some(l) = rows.last_mut() {
                l.1.push_str(text);
            }
        }
        (1..=n).filter_map(|i| rows.iter().find(|(_, t)| t.contains(&format!("#{i} "))).map(|(r, _)| *r)).collect()
    }

    #[test]
    fn blank_line_between_entries_when_they_fit() {
        let list: Vec<SessionInfo> = (1..=3).map(|i| info(i, "calm-otter", 0, 10 - i as u64)).collect();
        let mut sw = Switcher::new(list, None);
        let rows = entry_rows(&sw.render(80, 24, 100), 3);
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[1] - rows[0], rows[2] - rows[1]), (2, 2));
        // the row between is empty
        let out = sw.render(80, 24, 100);
        assert!(rows_of(&out).iter().any(|(r, t)| *r == rows[0] + 1 && t.trim_matches('│').trim().is_empty()));
    }

    #[test]
    fn compact_when_entries_do_not_fit_with_spacing() {
        // 12 entries: 12 rows fit in 80x24 but 12 + 11 blank rows do not.
        let list: Vec<SessionInfo> = (1..=12).map(|i| info(i, "calm-otter", 0, 100 - i as u64)).collect();
        let mut sw = Switcher::new(list, None);
        let out = sw.render(80, 24, 100);
        let rows = entry_rows(&out, 12);
        assert_eq!(rows.len(), 12);
        assert!(rows.windows(2).all(|w| w[1] - w[0] == 1), "{rows:?}");
        // 40x10 stays in bounds in both modes
        for n in [2u32, 9] {
            let list: Vec<SessionInfo> = (1..=n).map(|i| info(i, "calm-otter", 0, i as u64)).collect();
            let mut sw = Switcher::new(list, None);
            for _ in 0..n {
                let rows = rows_of(&sw.render(40, 10, 100));
                assert!(rows.iter().all(|(r, t)| *r <= 10 && t.chars().count() <= 40), "{rows:?}");
                sw.handle(Key::Ctrl('n'));
            }
        }
        let mut rows = entry_rows(&Switcher::new((1..=9).map(|i| info(i, "calm-otter", 0, i as u64)).collect(), None).render(40, 10, 100), 9);
        rows.sort();
        // the selected entry has its detail line below it; the others are adjacent
        assert!(rows.len() >= 3 && rows[1..].windows(2).all(|w| w[1] - w[0] == 1), "{rows:?}");
    }

    #[test]
    fn detach_chords_in_the_switcher() {
        let mk = || {
            let mut sw = Switcher::new(vec![info(1, "calm-otter", 0, 1)], None);
            sw.set_prefix(0x1d);
            sw
        };
        let mut sw = mk();
        assert_eq!(sw.handle(Key::Ctrl(']')), Action::None);
        assert_eq!(sw.handle(Key::Char('d')), Action::Detach);
        let mut sw = mk();
        assert_eq!(sw.handle(Key::Ctrl('x')), Action::None);
        assert_eq!(sw.handle(Key::Ctrl('c')), Action::Detach);
        // a plain `d` types into the filter; the chord is cancelled by another key
        let mut sw = mk();
        assert_eq!(sw.handle(Key::Char('d')), Action::None);
        assert_eq!(sw.filter.text(), "d");
        let mut sw = mk();
        sw.handle(Key::Ctrl(']'));
        sw.handle(Key::Char('x'));
        assert_eq!(sw.handle(Key::Char('d')), Action::None);
        assert_eq!(parse_keys(b"\x1d"), vec![Key::Ctrl(']')]);
        // the hint advertises it when wide enough
        let out = sw.render(120, 24, 100);
        assert!(out.contains("C-] d detach"));
        assert!(!sw.render(60, 24, 100).contains("detach"));
    }

    fn with_status(mut i: SessionInfo, st: &str, since: u64) -> SessionInfo {
        i.status = st.into();
        i.status_since = since;
        i
    }

    #[test]
    fn status_lines_colors_and_filter() {
        let a = with_recap(with_status(info(1, "calm-otter", 0, 50), "✓ cargo build · {ago}", 40), "nt reconnect: proto fixes");
        let b = with_status(info(2, "brave-lynx", 0, 10), "✗ cargo test (101) · {ago}", 90);
        let c = with_status(info(3, "icy-heron", 0, 5), "⏵ make deploy · {dur}", 70);
        let mut sw = Switcher::new(vec![a.clone(), b.clone(), c.clone()], None);
        let out = sw.render(80, 24, 100);
        // elapsed times are rendered from the timestamps; symbols are coloured
        assert!(out.contains("cargo build · 1m ago") && out.contains("cargo test (101) · 10s ago") && out.contains("make deploy · 30s"));
        assert!(out.contains("\x1b[32m✓") && out.contains("\x1b[31m✗") && out.contains("\x1b[33m⏵"));
        assert!(out.contains("nt reconnect: proto fixes"));
        // the filter also matches status and topic
        for ch in "deploy".chars() {
            sw.handle(Key::Char(ch));
        }
        let out = sw.render(80, 24, 100);
        assert!(out.contains("icy-heron") && !out.contains("calm-otter") && !out.contains("brave-lynx"));
        let mut sw = Switcher::new(vec![a, b, c], None);
        for ch in "proto fixes".chars() {
            sw.handle(Key::Char(ch));
        }
        let out = sw.render(80, 24, 100);
        assert!(out.contains("calm-otter") && !out.contains("icy-heron"));
    }

    #[test]
    fn tiny_terminal_shows_extras_only_for_the_selection() {
        let list: Vec<SessionInfo> = (1..=9)
            .map(|i| with_recap(with_status(info(i, "calm-otter", 0, i as u64), "✓ some status line · {ago}", 0), "topic text that is fairly long"))
            .collect();
        let mut sw = Switcher::new(list, None);
        for step in 0..12 {
            let out = sw.render(40, 10, 100);
            let rows = rows_of(&out);
            assert!(rows.iter().all(|(r, t)| *r <= 10 && t.chars().count() <= 40), "step {step}: {rows:?}");
            assert_eq!(out.matches("some status line").count(), 1, "step {step}");
            assert_eq!(out.matches("topic text").count(), 1, "step {step}");
            sw.handle(Key::Ctrl('n'));
        }
        // with room for the status line but not the topics, every entry shows its status
        let list: Vec<SessionInfo> =
            (1..=4).map(|i| with_recap(with_status(info(i, "calm-otter", 0, i as u64), "✓ ok · {ago}", 0), "topic text")).collect();
        let mut sw = Switcher::new(list, None);
        let out = sw.render(60, 13, 100);
        assert_eq!(out.matches(" ok · 1m ago").count(), 4);
        assert_eq!(out.matches("topic text").count(), 0);
    }

    fn with_load(mut i: SessionInfo, cpu: f32, rss: u64) -> SessionInfo {
        i.cpu_pct = cpu;
        i.rss_bytes = rss;
        i
    }

    fn load_list() -> Vec<SessionInfo> {
        vec![
            with_load(info(1, "calm-otter", 0, 50), 12.0, 340 << 20),
            with_load(info(2, "brave-lynx", 0, 40), 350.0, 100 << 20),
            with_load(info(3, "icy-heron", 0, 30), 0.0, 2 << 30),
        ]
    }

    #[test]
    fn usage_at_three_widths() {
        let mut sw = Switcher::new(load_list(), None);
        sw.ascii = false;
        // wide: bars and numbers, header total, busiest cpu in red
        let out = sw.render(100, 24, 100);
        assert!(out.contains("█") && out.contains(" 12%") && out.contains("340M") && out.contains("350%") && out.contains("2.0G"), "{out:?}");
        assert!(out.contains("3 sessions · cpu 362% · 2.4G"), "{out:?}");
        assert!(out.contains("\x1b[31m ███ "), "{out:?}");
        assert!(rows_of(&out).iter().all(|(_, t)| t.chars().count() <= 100));
        // medium: bars, no numbers
        let out = sw.render(60, 24, 100);
        assert!(out.contains("█") && !out.contains("340M") && !out.contains(" 12%"), "{out:?}");
        assert!(rows_of(&out).iter().all(|(_, t)| t.chars().count() <= 60));
        // narrow: heat glyphs and the selected entry's detail line
        let out = sw.render(40, 14, 100);
        assert!(!out.contains("█") && out.contains("▇") && out.contains("cpu "), "{out:?}");
        assert_eq!(out.matches("cpu 12% · 340M").count(), 1);
        assert!(rows_of(&out).iter().all(|(r, t)| *r <= 14 && t.chars().count() <= 40));
        let out = sw.render(40, 6, 100);
        assert!(rows_of(&out).iter().all(|(r, t)| *r <= 6 && t.chars().count() <= 40));
    }

    #[test]
    fn usage_ascii_fallback() {
        let mut sw = Switcher::new(load_list(), None);
        sw.ascii = true;
        let out = sw.render(100, 24, 100);
        assert!(out.contains("[###]") && !out.contains('█'), "{out:?}");
        let out = sw.render(40, 14, 100);
        assert!(out.contains('#') && !out.contains('▇'));
    }

    #[test]
    fn sort_cycles_and_is_shown() {
        let mut sw = Switcher::new(load_list(), None);
        let order = |sw: &Switcher| sw.visible().iter().map(|&i| sw.sessions[i].id).collect::<Vec<_>>();
        assert_eq!(order(&sw), vec![1, 2, 3]);
        sw.handle(Key::Alt('s'));
        assert_eq!(order(&sw), vec![2, 1, 3]);
        assert!(sw.render(100, 24, 100).contains("sort:cpu"));
        sw.handle(Key::Alt('s'));
        assert_eq!(order(&sw), vec![3, 1, 2]);
        assert!(sw.render(100, 24, 100).contains("sort:ram"));
        sw.handle(Key::Alt('s'));
        assert_eq!(order(&sw), vec![1, 2, 3]);
    }

    #[test]
    fn exited_sessions_are_marked() {
        let mut e = info(2, "brave-lynx", 0, 10);
        e.exited = true;
        let mut sw = Switcher::new(vec![info(1, "calm-otter", 0, 50), e], None);
        assert!(sw.render(80, 24, 100).contains("exited"));
        for c in "exited".chars() {
            sw.handle(Key::Char(c));
        }
        assert_eq!(sw.handle(Key::Enter), Action::Attach(2));
    }

    #[test]
    fn full_text_search_mode() {
        let list = vec![info(1, "calm-otter", 0, 50), info(2, "brave-lynx", 0, 10)];
        let mut sw = Switcher::new(list, None);
        sw.handle(Key::Ctrl('s'));
        assert!(sw.search_due().is_none() && sw.take_search().is_none());
        for c in "panic".chars() {
            sw.handle(Key::Char(c));
        }
        assert!(sw.search_due().is_some());
        assert_eq!(sw.take_search().as_deref(), Some("panic"));
        assert_eq!(sw.take_search(), None); // unchanged query is not re-sent
        sw.set_hits(vec![
            SearchHit { session: 2, line: "thread main panicked at src/x.rs".into() },
            SearchHit { session: 1, line: "another panic".into() },
        ]);
        let out = sw.render(40, 10, 100);
        assert!(out.contains("brave-lynx: thread main") && out.contains("nt search"));
        assert!(rows_of(&out).iter().all(|(r, t)| *r <= 10 && t.chars().count() <= 40));
        assert_eq!(sw.handle(Key::Enter), Action::Attach(2));
        sw.handle(Key::Ctrl('n'));
        assert_eq!(sw.handle(Key::Enter), Action::Attach(1));
        // back to the local filter
        sw.handle(Key::Ctrl('s'));
        assert!(sw.render(80, 24, 100).contains("nt sessions"));
    }

    fn with_summary(mut i: SessionInfo, t: &str) -> SessionInfo {
        i.summary = t.into();
        i
    }

    const SUMMARY: &str = "Working on the nt reconnect logic in ~/src/nt. Fixed a type error in proto.rs after a failed cargo test; tests now pass. Started deploying with make deploy.";

    /// The visible text of screen row `r` (1-based) after a render: cursor moves apply, other escapes are dropped.
    fn text_at(out: &str, r: usize) -> String {
        let (mut row, mut text) = (0, String::new());
        let mut it = out.chars().peekable();
        while let Some(c) = it.next() {
            if c != '\x1b' {
                if row == r {
                    text.push(c);
                }
                continue;
            }
            it.next(); // '['
            let mut params = String::new();
            while let Some(&n) = it.peek() {
                it.next();
                if n.is_ascii_alphabetic() {
                    if n == 'H' {
                        row = params.split(';').next().and_then(|p| p.parse().ok()).unwrap_or(0);
                    }
                    break;
                }
                params.push(n);
            }
        }
        text
    }

    #[test]
    fn detail_area_80x24_shows_the_selected_summary() {
        let list = vec![
            with_summary(info(1, "calm-otter", 0, 50), SUMMARY),
            with_summary(info(2, "brave-lynx", 0, 10), "Editing the nginx config on web02."),
        ];
        let mut sw = Switcher::new(list, None);
        let out = sw.render(80, 24, 100);
        let rows = rows_of(&out);
        // no per-row lines: the summaries only appear in the detail area (rows 20-22, above the hint on row 23)
        assert_eq!(out.matches("Working on the nt reconnect").count(), 1);
        assert_eq!(out.matches("nginx config").count(), 0);
        let d: Vec<String> = (20..=22).map(|r| text_at(&out, r)).collect();
        assert!(d[0].contains("Working on the nt reconnect logic in"), "{d:?}");
        assert!(d.join(" ").contains("make deploy."), "{d:?}");
        assert!(d.iter().all(|l| l.chars().count() == 80), "{d:?}");
        assert!(out.contains("\x1b[2m "), "dim");
        let hint = text_at(&out, 23);
        assert!(hint.contains("M-i info") && hint.contains("C-k kill"), "{hint}");
        // the box is unchanged: 24 rows, bottom border on row 24
        assert!(text_at(&out, 24).starts_with('└') && rows.iter().all(|(r, _)| *r <= 24));
        // moving the selection changes the detail text
        sw.handle(Key::Ctrl('n'));
        let out = sw.render(80, 24, 100);
        assert!(text_at(&out, 20).contains("Editing the nginx config on web02.") && !out.contains("Working on the nt"));
        // M-i hides and shows it again
        sw.handle(Key::Alt('i'));
        let out = sw.render(80, 24, 100);
        assert!(!out.contains("nginx config") && text_at(&out, 23).contains("M-i info"));
        sw.handle(Key::Alt('i'));
        assert!(sw.render(80, 24, 100).contains("nginx config"));
    }

    #[test]
    fn detail_area_is_capped_at_three_lines_and_hidden_when_short() {
        let long = "word ".repeat(80);
        let mut sw = Switcher::new(vec![with_summary(info(1, "calm-otter", 0, 50), &long), info(2, "brave-lynx", 0, 10)], None);
        let out = sw.render(80, 24, 100);
        assert!(text_at(&out, 22).contains('…') && text_at(&out, 23).contains("RET"), "{}", text_at(&out, 22));
        assert!(!text_at(&out, 19).contains("word"));
        // 40x10 (and anything under 20 rows): hidden, no hint for it
        for rows in [10u16, 19] {
            let out = sw.render(40, rows, 100);
            assert!(!out.contains("word"), "{rows}");
            assert!(!out.contains("M-i"), "{rows}");
        }
        // 40x20: shown, narrower, each line within the box
        let out = sw.render(40, 20, 100);
        assert!(out.contains("word word"));
        assert!(rows_of(&out).iter().all(|(_, t)| t.chars().count() <= 40));
        // a list without any summary never reserves the area
        let mut sw = Switcher::new(vec![info(1, "calm-otter", 0, 50)], None);
        assert!(!sw.render(80, 24, 100).contains("M-i"));
    }

    #[test]
    fn wrapping() {
        assert_eq!(wrap("aaa bbb ccc ddd", 7, 5), vec!["aaa bbb", "ccc ddd"]);
        assert_eq!(wrap("aaa bbb ccc ddd", 7, 1), vec!["aaa bb…"]);
        assert_eq!(wrap("", 7, 3), Vec::<String>::new());
        assert_eq!(wrap("abcdefghij", 4, 5), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn filter_matches_summary_as_substring() {
        let list = vec![
            with_summary(info(1, "calm-otter", 0, 50), SUMMARY),
            with_summary(info(2, "brave-lynx", 0, 10), "Editing the nginx config on web02."),
        ];
        let mut sw = Switcher::new(list, None);
        for c in "nginx".chars() {
            sw.handle(Key::Char(c));
        }
        assert_eq!(sw.handle(Key::Enter), Action::Attach(2));
        // a term spread over the letters of the prose does not match; a mix of name and summary terms does
        sw.handle(Key::Ctrl('u'));
        for c in "rcnx".chars() {
            sw.handle(Key::Char(c));
        }
        assert_eq!(sw.visible().len(), 0);
        sw.handle(Key::Ctrl('u'));
        for c in "otter deploy".chars() {
            sw.handle(Key::Char(c));
        }
        assert_eq!(sw.handle(Key::Enter), Action::Attach(1));
    }

    #[test]
    fn c_k_kills_at_the_end_of_the_filter_and_edits_elsewhere() {
        let mut exited = info(3, "icy-heron", 0, 5);
        exited.exited = true;
        let list = vec![info(1, "calm-otter", 0, 50), info(2, "brave-lynx", 0, 10), exited];
        let mut sw = Switcher::new(list, None);
        // empty filter: the cursor is at the end, so C-k kills the selected (live) session after y/n
        assert_eq!(sw.handle(Key::Ctrl('k')), Action::None);
        assert_eq!(sw.handle(Key::Char('n')), Action::None);
        sw.handle(Key::Ctrl('k'));
        assert_eq!(sw.handle(Key::Char('y')), Action::Kill(1));
        // filter "l" matches calm-otter, brave-lynx, icy-heron? no: only names/cmds with an l
        sw.handle(Key::Char('l'));
        sw.handle(Key::Char('y'));
        assert_eq!(sw.filter.text(), "ly");
        // cursor in the middle: C-k is kill-line (no kill prompt, text cut)
        sw.handle(Key::Ctrl('b'));
        assert_eq!(sw.handle(Key::Ctrl('k')), Action::None);
        assert_eq!(sw.filter.text(), "l");
        assert!(matches!(sw.sub, Sub::Browse));
        // at the end again: kills the selected one (brave-lynx matches "l")
        assert_eq!(sw.handle(Key::Ctrl('k')), Action::None);
        assert!(matches!(sw.sub, Sub::ConfirmKill(_)));
        assert_eq!(sw.handle(Key::Char('x')), Action::None);
        // M-k is an alias
        sw.handle(Key::Alt('k'));
        assert!(matches!(sw.sub, Sub::ConfirmKill(_)));
        sw.handle(Key::Char('n'));
        // an exited session closes at once, without y/n (both keys)
        sw.handle(Key::Ctrl('u'));
        for c in "icy".chars() {
            sw.handle(Key::Char(c));
        }
        assert_eq!(sw.handle(Key::Ctrl('k')), Action::Kill(3));
        assert_eq!(sw.handle(Key::Alt('k')), Action::Kill(3));
        // in full-text search mode C-k edits the query
        sw.handle(Key::Ctrl('s'));
        sw.handle(Key::Ctrl('a'));
        sw.handle(Key::Ctrl('k'));
        assert_eq!(sw.filter.text(), "");
        assert!(matches!(sw.sub, Sub::Browse));
    }

    #[test]
    fn search_hit_with_summary_label_is_listed() {
        let mut sw = Switcher::new(vec![with_summary(info(1, "calm-otter", 0, 50), SUMMARY)], None);
        sw.handle(Key::Ctrl('s'));
        sw.handle(Key::Char('r'));
        sw.set_hits(vec![SearchHit { session: 1, line: "summary: …the nt reconnect logic".into() }]);
        let out = sw.render(80, 24, 100);
        assert!(out.contains("calm-otter: summary: …the nt reconnect logic"));
    }
}
