//! Plain-text session transcripts: a `vte`-based stripper turns terminal output into lines, a buffered
//! writer appends them to `~/.local/state/nt/transcripts/<created>-<id>.txt` (capped), and `search`
//! scans those files. A `.meta` file next to each transcript keeps the name, topic (`recap`) and summary.

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use vte::{Params, Parser, Perform};

use super::shell::{parse_osc, ShellEv};
use crate::proto::{NameSource, SearchHit, SessionId};

/// A file bigger than this is cut back to its last `KEEP` bytes.
pub const CAP: u64 = 4 << 20;
pub const KEEP: u64 = 2 << 20;
const MAX_LINE: usize = 2000;
const FLUSH_BYTES: usize = 64 << 10;

// ---------------------------------------------------------------- stripper

/// What the stripper saw, in stream order: a finished non-empty line or a shell-integration event.
#[derive(Debug, PartialEq, Eq)]
pub enum Item {
    Line(String),
    Ev(ShellEv),
}

#[derive(Default)]
struct Lines {
    items: Vec<Item>,
    line: Vec<char>,
    cur: usize,
    alt: bool,
    alt_entered: bool,
    out: String,
    blank_run: u32,
    /// Redraw operations (cursor up / home, screen erase, overwriting a line) since the last `take_noise`.
    noise: u32,
    overwrote: bool,
}

impl Lines {
    fn flush_line(&mut self) {
        self.overwrote = false;
        let s: String = self.line.iter().collect();
        let s = s.trim_end();
        if s.is_empty() {
            self.blank_run += 1;
            if self.blank_run > 1 {
                self.line.clear();
                self.cur = 0;
                return;
            }
        } else {
            self.blank_run = 0;
            self.items.push(Item::Line(s.to_string()));
            self.out.push_str(s);
        }
        self.out.push('\n');
        self.line.clear();
        self.cur = 0;
    }

    fn pad_to_cursor(&mut self) {
        while self.line.len() < self.cur.min(MAX_LINE) {
            self.line.push(' ');
        }
    }
}

impl Perform for Lines {
    fn print(&mut self, c: char) {
        if self.alt || self.cur >= MAX_LINE {
            return;
        }
        self.pad_to_cursor();
        if self.cur < self.line.len() {
            if self.line[self.cur] != c && !self.overwrote {
                self.overwrote = true;
                self.noise += 1;
            }
            self.line[self.cur] = c;
        } else {
            self.line.push(c);
        }
        self.cur += 1;
    }

    fn execute(&mut self, byte: u8) {
        if self.alt {
            return;
        }
        match byte {
            b'\n' | 0x0b | 0x0c => self.flush_line(),
            b'\r' => self.cur = 0,
            0x08 => self.cur = self.cur.saturating_sub(1),
            b'\t' => self.cur = ((self.cur / 8) + 1) * 8,
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell: bool) {
        if let Some(ev) = parse_osc(params) {
            self.items.push(Item::Ev(ev));
        }
    }

    fn csi_dispatch(&mut self, params: &Params, inter: &[u8], _ignore: bool, c: char) {
        let p0 = params.iter().next().and_then(|p| p.first()).copied().unwrap_or(0) as usize;
        if inter == b"?" {
            if matches!(c, 'h' | 'l') && params.iter().flatten().any(|&p| matches!(p, 47 | 1047 | 1049)) {
                if c == 'h' {
                    if !self.line.is_empty() {
                        self.flush_line();
                    }
                    self.alt = true;
                    self.alt_entered = true;
                } else {
                    self.alt = false;
                    self.line.clear();
                    self.cur = 0;
                }
            }
            return;
        }
        if self.alt || !inter.is_empty() {
            return;
        }
        if matches!(c, 'A' | 'H' | 'f' | 'J') || (c == 'K' && p0 == 2) {
            self.noise += 1;
        }
        match c {
            'K' => match p0 {
                0 => self.line.truncate(self.cur),
                1 => (0..(self.cur + 1).min(self.line.len())).for_each(|i| self.line[i] = ' '),
                _ => self.line.clear(),
            },
            'C' => self.cur = (self.cur + p0.max(1)).min(MAX_LINE),
            'D' => self.cur = self.cur.saturating_sub(p0.max(1)),
            'G' => self.cur = p0.max(1) - 1,
            'P' => {
                let end = (self.cur + p0.max(1)).min(self.line.len());
                if self.cur < end {
                    self.line.drain(self.cur..end);
                }
            }
            _ => {}
        }
    }
}

/// Terminal output in, plain completed lines out.
#[derive(Default)]
pub struct Stripper {
    parser: Parser,
    st: Lines,
}

impl Stripper {
    pub fn feed(&mut self, data: &[u8]) {
        self.parser.advance(&mut self.st, data);
    }

    /// Completed lines since the last call.
    pub fn take(&mut self) -> String {
        std::mem::take(&mut self.st.out)
    }

    /// True once after the alternate screen was entered.
    pub fn take_alt_entered(&mut self) -> bool {
        std::mem::take(&mut self.st.alt_entered)
    }

    /// Redraw operations seen since the last call (a measure of TUI noise).
    pub fn take_noise(&mut self) -> u32 {
        std::mem::take(&mut self.st.noise)
    }

    /// Lines and shell events since the last call.
    pub fn take_items(&mut self) -> Vec<Item> {
        std::mem::take(&mut self.st.items)
    }

    pub fn in_alt(&self) -> bool {
        self.st.alt
    }

    /// The unfinished current line.
    pub fn partial(&self) -> String {
        self.st.line.iter().collect::<String>().trim_end().to_string()
    }
}

// ---------------------------------------------------------------- writer

pub fn dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(base.join("nt/transcripts"))
}

pub struct Transcript {
    stripper: Stripper,
    path: Option<PathBuf>,
    buf: String,
    /// Size of the file on disk.
    size: u64,
    /// Total bytes ever appended (buffered ones included); a stable position for "new since".
    written: u64,
    dir_ready: bool,
    pub last_flush: Instant,
}

impl Transcript {
    pub fn new(dir: Option<&Path>, created: u64, id: SessionId) -> Transcript {
        Transcript {
            stripper: Stripper::default(),
            path: dir.map(|d| d.join(format!("{created}-{id}.txt"))),
            buf: String::new(),
            size: 0,
            written: 0,
            dir_ready: false,
            last_flush: Instant::now(),
        }
    }

    pub fn take_items(&mut self) -> Vec<Item> {
        self.stripper.take_items()
    }

    pub fn in_alt(&self) -> bool {
        self.stripper.in_alt()
    }

    pub fn take_noise(&mut self) -> u32 {
        self.stripper.take_noise()
    }

    pub fn partial(&self) -> String {
        self.stripper.partial()
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    pub fn feed(&mut self, data: &[u8]) {
        self.stripper.feed(data);
        self.push_out();
    }

    fn push_out(&mut self) {
        let s = self.stripper.take();
        self.written += s.len() as u64;
        self.buf.push_str(&s);
        if self.buf.len() >= FLUSH_BYTES {
            self.flush();
        }
    }

    /// `[fg_cmd in cwd]` once per alternate-screen entry (nothing else is recorded while it is active).
    pub fn alt_entered(&mut self) -> bool {
        self.stripper.take_alt_entered()
    }

    pub fn marker(&mut self, text: &str) {
        let line = format!("[{}]\n", text.chars().filter(|c| !c.is_control()).collect::<String>());
        self.written += line.len() as u64;
        self.buf.push_str(&line);
    }

    /// Append the buffer to the file; cut the file back when it exceeds the cap.
    pub fn flush(&mut self) {
        self.last_flush = Instant::now();
        let Some(path) = &self.path else { return };
        if self.buf.is_empty() {
            return;
        }
        if !self.dir_ready {
            if let Some(d) = path.parent() {
                let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(d);
            }
            self.dir_ready = true;
        }
        let r = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .and_then(|mut f| f.write_all(self.buf.as_bytes()));
        if r.is_ok() {
            self.size += self.buf.len() as u64;
        }
        self.buf.clear();
        if self.size > CAP {
            self.size = trim_file(path).unwrap_or(self.size);
        }
    }

    /// Flush the partial line too (session end).
    pub fn finish(&mut self) {
        let p = self.stripper.partial();
        if !p.is_empty() {
            self.buf.push_str(&p);
            self.buf.push('\n');
            self.written += p.len() as u64 + 1;
        }
        self.flush();
    }

    /// Last `max` bytes of the transcript (starting at a line boundary), flushing first.
    pub fn tail(&mut self, max: usize) -> String {
        self.flush();
        let Some(path) = &self.path else { return String::new() };
        read_tail(path, max).unwrap_or_default()
    }

    pub fn write_meta(&self, name: &str, source: NameSource, recap: &str, summary: &str) {
        let Some(path) = &self.path else { return };
        if let Some(d) = path.parent() {
            let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(d);
        }
        let one = |s: &str| s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>();
        let text = format!("name={}\nname_source={:?}\nrecap={}\nsummary={}\n", one(name), source, one(recap), one(summary));
        let _ = std::fs::write(path.with_extension("meta"), text);
    }
}

fn read_tail(path: &Path, max: usize) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max as u64);
    f.seek(SeekFrom::Start(start))?;
    let mut v = Vec::new();
    f.read_to_end(&mut v)?;
    let s = String::from_utf8_lossy(&v).into_owned();
    Ok(if start > 0 { s.split_once('\n').map_or(String::new(), |(_, rest)| rest.to_string()) } else { s })
}

/// Keep the last `KEEP` bytes (from a line start). Returns the new size.
fn trim_file(path: &Path) -> std::io::Result<u64> {
    let keep = read_tail(path, KEEP as usize)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &keep)?;
    std::fs::rename(&tmp, path)?;
    Ok(keep.len() as u64)
}

// ---------------------------------------------------------------- search

/// A short excerpt of `line` that starts a little before the match at char index `at`.
fn excerpt(line: &str, at: usize) -> String {
    let start = at.saturating_sub(20);
    let body: String = line.chars().skip(start).take(140).collect();
    if start > 0 { format!("…{body}") } else { body }
}

/// Case-insensitive substring search, newest sessions and lines first. At most 3 hits per session and
/// 50 overall. Only sessions in `live` (`(created_unix, id, summary)`) are searched; a match in the summary
/// comes first and is shown as `summary: …snippet`.
pub fn search(dir: &Path, live: &[(u64, SessionId, String)], query: &str) -> Vec<SearchHit> {
    let q: String = query.trim().chars().take(200).collect::<String>().to_lowercase();
    let mut hits = Vec::new();
    if q.is_empty() {
        return hits;
    }
    let mut live: Vec<(u64, SessionId, String)> = live.to_vec();
    live.sort_by(|a, b| b.cmp(a));
    for (created, id, summary) in live {
        let mut n = 0;
        let low = summary.to_lowercase();
        if let Some(pos) = low.find(&q) {
            let shown = if low.len() == summary.len() { summary.as_str() } else { &low };
            hits.push(SearchHit { session: id, line: format!("summary: {}", excerpt(shown, shown[..pos].chars().count())) });
            n += 1;
        }
        let Ok(text) = std::fs::read(dir.join(format!("{created}-{id}.txt"))) else { continue };
        let text = String::from_utf8_lossy(&text);
        for line in text.lines().rev() {
            let low = line.to_lowercase();
            let Some(pos) = low.find(&q) else { continue };
            let shown = if low.len() == line.len() { line } else { &low };
            hits.push(SearchHit { session: id, line: excerpt(shown, shown[..pos].chars().count()) });
            n += 1;
            if n == 3 || hits.len() >= 50 {
                break;
            }
        }
        if hits.len() >= 50 {
            break;
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(chunks: &[&[u8]]) -> String {
        let mut s = Stripper::default();
        for c in chunks {
            s.feed(c);
        }
        let mut out = s.take();
        out.push_str(&s.partial());
        out
    }

    #[test]
    fn plain_lines_and_colors() {
        assert_eq!(strip(&[b"\x1b[1;31merror\x1b[0m: bad\r\nok\r\n"]), "error: bad\nok\n");
        // escape split across chunks
        assert_eq!(strip(&[b"a\x1b[3", b"1mb\x1b[0m\r\n"]), "ab\n");
        // OSC title dropped
        assert_eq!(strip(&[b"\x1b]0;title\x07hi\r\n"]), "hi\n");
    }

    #[test]
    fn carriage_return_overwrites() {
        assert_eq!(strip(&[b"10%\r50%\r100%\r\ndone\r\n"]), "100%\ndone\n");
        // a shorter overwrite leaves the old tail, like a real terminal
        assert_eq!(strip(&[b"abcdef\rXY\r\n"]), "XYcdef\n");
    }

    #[test]
    fn backspace_and_erase() {
        assert_eq!(strip(&[b"abc\x08\x08XY\r\n"]), "aXY\n");
        assert_eq!(strip(&[b"abc\x08 \x08\x08 \x08\r\n"]), "a\n");
        assert_eq!(strip(&[b"hello\x1b[2K\rbye\r\n"]), "bye\n");
        assert_eq!(strip(&[b"hello\r\x1b[Kbye\r\n"]), "bye\n");
        assert_eq!(strip(&[b"hello world\x1b[6D\x1b[K\r\n"]), "hello\n");
        assert_eq!(strip(&[b"hello\x1b[1K\r\n"]), "\n");
    }

    #[test]
    fn alt_screen_is_skipped() {
        let mut s = Stripper::default();
        s.feed(b"before\r\n\x1b[?1049h\x1b[2J\x1b[Hvim junk\r\nmore\x1b[?1049l");
        assert!(s.take_alt_entered());
        assert!(!s.take_alt_entered());
        assert!(!s.in_alt());
        s.feed(b"after\r\n");
        assert_eq!(s.take(), "before\nafter\n");
    }

    #[test]
    fn blank_runs_collapse_and_tabs() {
        assert_eq!(strip(&[b"a\r\n\r\n\r\n\r\nb\r\n"]), "a\n\nb\n");
        assert_eq!(strip(&[b"a\tb\r\n"]), "a       b\n");
    }

    #[test]
    fn utf8_split_across_chunks() {
        assert_eq!(strip(&[b"caf\xc3", b"\xa9\r\n"]), "café\n");
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nt-tr-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn writer_cap_and_tail() {
        let d = tmpdir("cap");
        let mut t = Transcript::new(Some(&d), 1, 1);
        let line = format!("{}\r\n", "x".repeat(99));
        for _ in 0..(CAP as usize / 100 + 500) {
            t.feed(line.as_bytes());
        }
        t.flush();
        let size = std::fs::metadata(d.join("1-1.txt")).unwrap().len();
        assert!(size <= KEEP + 1000 || size <= CAP, "size {size}");
        assert!(t.written() > CAP);
        t.feed(b"final line\r\n");
        assert!(t.tail(100).ends_with("final line\n"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn search_finds_newest_first() {
        let d = tmpdir("search");
        let mut a = Transcript::new(Some(&d), 100, 1);
        a.feed(b"one Needle here\r\nnothing\r\ntwo needle there\r\n");
        a.flush();
        let mut b = Transcript::new(Some(&d), 200, 2);
        b.feed(b"NEEDLE in b\r\n");
        b.flush();
        let hits = search(&d, &[(100, 1, String::new()), (200, 2, String::new())], "needle");
        assert_eq!(hits.len(), 3);
        assert_eq!((hits[0].session, hits[0].line.as_str()), (2, "NEEDLE in b"));
        assert_eq!(hits[1].line, "two needle there");
        assert!(search(&d, &[(100, 1, String::new())], "  ").is_empty());
        assert!(search(&d, &[(999, 9, String::new())], "needle").is_empty());
        // summaries are searched too: first, labelled, with a snippet around the match; also without a transcript
        let sum = "Working on the nt reconnect logic in ~/src/nt, fixed a type error in proto.rs after a failed cargo test.".to_string();
        let hits = search(&d, &[(100, 1, "A NEEDLE appears".into()), (999, 9, sum)], "RECONNECT");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session, 9);
        assert!(hits[0].line.starts_with("summary: ") && hits[0].line.contains("reconnect logic"), "{}", hits[0].line);
        let hits = search(&d, &[(100, 1, "A NEEDLE appears".into())], "needle");
        assert_eq!((hits[0].line.as_str(), hits.len()), ("summary: A NEEDLE appears", 3));
        let _ = std::fs::remove_dir_all(&d);
    }

    fn titles(chunks: &[&[u8]]) -> Vec<ShellEv> {
        let mut s = Stripper::default();
        for c in chunks {
            s.feed(c);
        }
        s.take_items().into_iter().filter_map(|i| if let Item::Ev(e) = i { Some(e) } else { None }).collect()
    }

    #[test]
    fn osc_titles() {
        let t = |s: &str| ShellEv::Title(s.into());
        assert_eq!(titles(&[b"\x1b]0;one\x07x\x1b]2;two\x1b\\y"]), [t("one"), t("two")]);
        // split across reads, inside the sequence and in a UTF-8 glyph
        assert_eq!(titles(&[b"\x1b]0;\xe2\x9c", b"\xb3 Fix bug", b"\x07", b"\x1b]2;next\x1b", b"\\"]), [t("✳ Fix bug"), t("next")]);
    }

    #[test]
    fn tui_redraws_count_as_noise() {
        let mut s = Stripper::default();
        for i in 0..30 {
            s.feed(format!("\x1b[2K\x1b[1A\x1b[2K\r spinner {i}\r\n").as_bytes());
        }
        assert!(s.take_noise() >= 60);
        s.feed(b"plain\r\nlines\r\n");
        assert_eq!(s.take_noise(), 0);
    }
}
