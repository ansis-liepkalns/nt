//! Shell integration: the hook scripts nt injects into bash, zsh and fish, the OSC 133 / OSC 7
//! sequences they emit, the per-session command log built from those, and the live status string
//! and LLM digest derived from the log.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use super::naming::find_repo;

// ---------------------------------------------------------------- hook files

const BASH_INIT: &str = include_str!("hooks/bash-init");
const ZSHENV: &str = include_str!("hooks/zshenv");
const ZPROFILE: &str = include_str!("hooks/zprofile");
const ZSHRC: &str = include_str!("hooks/zshrc");
const FISH_INIT: &str = include_str!("hooks/fish-init.fish");

/// Where the hook files live (inside the runtime dir).
pub struct Hooks {
    pub bash_init: PathBuf,
    pub zdotdir: PathBuf,
    pub fish_init: PathBuf,
}

fn write_if_changed(path: &Path, content: &str) -> Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|c| c == content) {
        return Ok(());
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, content).with_context(|| tmp.display().to_string())?;
    std::fs::rename(&tmp, path).with_context(|| path.display().to_string())
}

/// Write the hook files under `dir` (the runtime dir). Cheap enough to call on every spawn.
pub fn install(dir: &Path) -> Result<Hooks> {
    let zdotdir = dir.join("zdotdir");
    std::fs::create_dir_all(&zdotdir)?;
    let h = Hooks { bash_init: dir.join("bash-init"), fish_init: dir.join("fish-init.fish"), zdotdir };
    write_if_changed(&h.bash_init, BASH_INIT)?;
    write_if_changed(&h.fish_init, FISH_INIT)?;
    write_if_changed(&h.zdotdir.join(".zshenv"), ZSHENV)?;
    write_if_changed(&h.zdotdir.join(".zprofile"), ZPROFILE)?;
    write_if_changed(&h.zdotdir.join(".zshrc"), ZSHRC)?;
    Ok(h)
}

/// The command that starts `shell` as a login shell, with the integration hook when the shell is known.
pub fn shell_command(shell: &str, hooks: Option<&Hooks>) -> Command {
    let mut cmd = Command::new(shell);
    let name = Path::new(shell).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match (name.as_str(), hooks) {
        // `bash -l` ignores --rcfile; bash-init does the login-shell file lookup itself.
        ("bash", Some(h)) => {
            cmd.arg("--rcfile").arg(&h.bash_init).arg("-i");
        }
        ("zsh", Some(h)) => {
            if let Some(z) = std::env::var_os("ZDOTDIR").filter(|z| !z.is_empty()) {
                cmd.env("NT_ORIG_ZDOTDIR", z);
            } else {
                cmd.env_remove("NT_ORIG_ZDOTDIR");
            }
            cmd.env("ZDOTDIR", &h.zdotdir).arg("-l");
        }
        ("fish", Some(h)) => {
            let p = h.fish_init.display().to_string().replace('\\', "\\\\").replace('\'', "\\'");
            cmd.arg("-l").arg("--init-command").arg(format!("source '{p}'"));
        }
        _ => {
            cmd.arg("-l");
        }
    }
    cmd
}

// ---------------------------------------------------------------- OSC parsing

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEv {
    /// 133;A
    Prompt,
    /// 133;E;<cmd>
    CmdLine(String),
    /// 133;C
    OutputStart,
    /// 133;D[;<exit>]
    Done(Option<i32>),
    /// 7;file://host/path
    Cwd(String),
    /// OSC 0 / OSC 2: the terminal title, sanitized (control characters dropped, trimmed, at most `MAX_TITLE` chars).
    Title(String),
}

pub const MAX_TITLE: usize = 120;

/// Control characters dropped, whitespace collapsed, capped.
pub fn sanitize_title(raw: &str) -> String {
    let clean: String = raw.chars().filter(|c| !c.is_control()).collect();
    clean.trim().chars().take(MAX_TITLE).collect::<String>().trim_end().to_string()
}

/// The title without a leading decorative glyph or spinner (`✳ `, `⠂ `, `· `, `* `): the text that says what is going on.
pub fn title_text(t: &str) -> &str {
    let t = t.trim();
    let mut it = t.chars();
    match (it.next(), it.next()) {
        (Some(g), Some(sp)) if !g.is_alphanumeric() && sp.is_whitespace() && !matches!(g, '/' | '~' | '.' | '[' | '(' | '$' | '#' | '>') => it.as_str().trim_start(),
        _ => t,
    }
}

/// Does `t` look like what a shell sets by default (`user@host: ~/dir`, the directory itself, the command line)?
pub fn is_default_title(t: &str, cwd: &str, home: &str, cmd: &str) -> bool {
    let t = title_text(t);
    if t.is_empty() {
        return true;
    }
    if !cwd.is_empty() && (t == cwd || t == tilde(cwd, home)) {
        return true;
    }
    if !cmd.is_empty() && (t == cmd || Some(t) == cmd.split_whitespace().next()) {
        return true;
    }
    if let Some((who, path)) = t.split_once(':') {
        let path = path.trim_start();
        let host = who.split_once('@');
        if host.is_some_and(|(u, h)| !u.is_empty() && !h.is_empty() && !who.contains(' ')) && (path.starts_with(['~', '/']) || path.is_empty()) {
            return true;
        }
    }
    false
}

/// Decode `\xNN` and `\\` escapes (bytes may form UTF-8), then drop control characters.
pub fn unescape_cmd(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 4 <= b.len() && b[i + 1] == b'x' {
            if let Some(v) = std::str::from_utf8(&b[i + 2..i + 4]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 4;
                continue;
            }
        } else if b[i] == b'\\' && i + 1 < b.len() && b[i + 1] == b'\\' {
            out.push(b'\\');
            i += 2;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    let text = String::from_utf8_lossy(&out);
    let clean: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    clean.trim().chars().take(500).collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 3 <= b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// An OSC sequence (already split at `;`) -> event, if it is one we care about.
pub fn parse_osc(params: &[&[u8]]) -> Option<ShellEv> {
    let s = |p: &[u8]| String::from_utf8_lossy(p).into_owned();
    match params.first().copied()? {
        b"133" => match params.get(1).copied()? {
            b"A" => Some(ShellEv::Prompt),
            b"C" => Some(ShellEv::OutputStart),
            b"D" => Some(ShellEv::Done(params.get(2).and_then(|p| s(p).trim().parse().ok()))),
            b"E" => {
                let raw = params[2..].iter().map(|p| s(p)).collect::<Vec<_>>().join(";");
                Some(ShellEv::CmdLine(unescape_cmd(&raw)))
            }
            _ => None,
        },
        b"0" | b"2" => {
            let raw = params[1..].iter().map(|p| s(p)).collect::<Vec<_>>().join(";");
            Some(ShellEv::Title(sanitize_title(&raw)))
        }
        b"7" => {
            let url = params[1..].iter().map(|p| s(p)).collect::<Vec<_>>().join(";");
            let rest = url.strip_prefix("file://")?;
            let path = &rest[rest.find('/')?..];
            Some(ShellEv::Cwd(percent_decode(path)))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------- command log

const MAX_ENTRIES: usize = 50;
pub const ERROR_WORDS: &[&str] = &["error", "failed", "denied", "refused", "not found", "panic"];

pub fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    /// 1-based running number of this command in the session (for "everything since the last summary").
    pub seq: u64,
    pub cmd: String,
    pub cwd: String,
    pub start: u64,
    pub end: Option<u64>,
    pub exit: Option<i32>,
    /// At most 3 error-looking output lines.
    pub errors: Vec<String>,
    /// Last non-empty output line.
    pub last: String,
}

#[derive(Default)]
pub struct CommandLog {
    entries: VecDeque<Cmd>,
    running: bool,
    /// Commands started so far (the `seq` of the newest entry).
    total: u64,
    /// Current directory from OSC 7.
    pub cwd: String,
    /// Seen at least one OSC 133 event: the hooks work.
    pub integrated: bool,
    /// Meaningful title of the program running now (empty: none; cleared when the command ends).
    pub title: String,
}

impl CommandLog {
    #[cfg(test)]
    pub fn entries(&self) -> &VecDeque<Cmd> {
        &self.entries
    }

    /// Number of commands started so far.
    #[cfg(test)]
    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    fn start(&mut self, cmd: String, now: u64) {
        self.total += 1;
        self.entries.push_back(Cmd { seq: self.total, cmd, cwd: self.cwd.clone(), start: now, end: None, exit: None, errors: Vec::new(), last: String::new() });
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_front();
        }
        self.running = true;
    }

    fn finish(&mut self, exit: Option<i32>, now: u64) {
        if let Some(c) = self.entries.back_mut().filter(|_| self.running) {
            c.end = Some(now);
            c.exit = exit;
        }
        self.running = false;
        self.title.clear();
    }

    /// Apply a shell event. Duplicates (fish emits OSC 133 itself) are harmless.
    pub fn apply(&mut self, ev: ShellEv, now: u64) {
        match ev {
            ShellEv::Cwd(p) => self.cwd = p,
            ShellEv::Title(t) => {
                self.note_title(&t, None, "");
            }
            ShellEv::Prompt => {
                self.integrated = true;
                if self.running {
                    self.finish(None, now);
                }
            }
            ShellEv::CmdLine(c) => {
                self.integrated = true;
                if self.running {
                    if let Some(e) = self.entries.back_mut().filter(|e| e.cmd.is_empty()) {
                        e.cmd = c;
                    }
                } else {
                    self.start(c, now);
                }
            }
            ShellEv::OutputStart => {
                self.integrated = true;
                if !self.running {
                    self.start(String::new(), now);
                }
            }
            ShellEv::Done(code) => {
                self.integrated = true;
                self.finish(code, now);
            }
        }
    }

    /// A title set by the program on the terminal. `shell_fg`: without shell integration, whether the shell itself is
    /// the foreground process (then it is the shell's default title). Returns true if it changed the title
    /// meaningfully (not just the spinner glyph).
    pub fn note_title(&mut self, raw: &str, shell_fg: Option<bool>, home: &str) -> bool {
        let raw = sanitize_title(raw);
        if (self.integrated && !self.running) || shell_fg == Some(true) {
            return false;
        }
        let cmd = self.running_cmd().map_or("", |c| c.cmd.as_str());
        if is_default_title(&raw, &self.cwd, home, cmd) {
            return false;
        }
        let text = title_text(&raw).to_string();
        if text == self.title {
            return false;
        }
        self.title = text;
        true
    }

    /// An output line (only meaningful while a command runs).
    pub fn line(&mut self, text: &str) {
        if !self.running {
            return;
        }
        let Some(c) = self.entries.back_mut() else { return };
        let t = text.trim();
        if t.is_empty() {
            return;
        }
        c.last = clip(t, 200);
        let low = t.to_lowercase();
        if c.errors.len() < 3 && ERROR_WORDS.iter().any(|w| low.contains(w)) {
            c.errors.push(clip(t, 120));
        }
    }

    pub fn running_cmd(&self) -> Option<&Cmd> {
        self.entries.back().filter(|_| self.running)
    }

    pub fn last_cmd(&self) -> Option<&Cmd> {
        self.entries.back()
    }

    /// The model's one line from the hooks: `running command: make -j8`, `last command: make -j8 · exit 2` or
    /// `… · ok`. Empty without hooks or commands.
    pub fn last_command_line(&self) -> String {
        let Some(c) = self.entries.back().filter(|c| !c.cmd.is_empty()) else { return String::new() };
        let cmd = clip(&c.cmd, 80);
        match (c.end, c.exit) {
            (None, _) => format!("running command: {cmd}"),
            (Some(_), Some(x)) if x != 0 => format!("last command: {cmd} · exit {x}"),
            (Some(_), _) => format!("last command: {cmd} · ok"),
        }
    }
}

// ---------------------------------------------------------------- status

/// Replace `~`-prefix of `path` by `~` when under `home`.
pub fn tilde(path: &str, home: &str) -> String {
    if !home.is_empty() && home != "/" {
        if path == home {
            return "~".into();
        }
        if let Some(r) = path.strip_prefix(home).filter(|r| r.starts_with('/')) {
            return format!("~{r}");
        }
    }
    path.to_string()
}

pub struct StatusIn<'a> {
    pub alt: bool,
    /// Foreground process name (from /proc), empty if unknown.
    pub fg_cmd: &'a str,
    /// The shell itself is the foreground process.
    pub fg_is_shell: bool,
    pub cwd: &'a str,
    pub home: &'a str,
    /// The unfinished output line.
    pub partial: &'a str,
    pub exited: bool,
}

const EDITORS: &[&str] = &["vim", "vi", "nvim", "nano", "emacs", "hx", "helix", "micro", "kak", "pico", "view", "ed"];

fn is_editor(cmd: &str) -> bool {
    let first = cmd.split_whitespace().next().unwrap_or("");
    EDITORS.contains(&first.rsplit('/').next().unwrap_or(first))
}

impl CommandLog {
    /// The status line and the unix time `{dur}` / `{ago}` count from (0 = none).
    /// `{dur}` is replaced by the elapsed time, `{ago}` by "N ago" on the client (`proto::expand_status`).
    pub fn status(&self, i: &StatusIn) -> (String, u64) {
        let idle = || format!("idle · {}", tilde(if i.cwd.is_empty() { &self.cwd } else { i.cwd }, i.home));
        if i.exited {
            return ("exited".into(), 0);
        }
        if !self.integrated {
            // No hooks: guess from the foreground process.
            return match (i.fg_is_shell, i.fg_cmd) {
                (_, "") | (true, _) => (idle(), 0),
                (false, c) => (format!("{} {c}{}", if i.alt && is_editor(c) { '✎' } else { '⏵' }, self.title_part()), 0),
            };
        }
        if let Some(c) = self.running_cmd() {
            let cmd = if c.cmd.is_empty() { i.fg_cmd } else { &c.cmd };
            let cmd = clip(cmd, 60);
            let title = self.title_part();
            if i.alt {
                return (format!("{} {cmd}{title}", if is_editor(&cmd) { '✎' } else { '⏵' }), c.start);
            }
            if !title.is_empty() {
                // The title says more than the redrawn output.
                return (format!("⏵ {cmd}{title} · {{dur}}"), c.start);
            }
            // A bare prompt sign (a nested `sh` waiting at `$ `) says nothing; show the last output line instead.
            let partial = i.partial.trim();
            let bare_prompt = !partial.is_empty() && partial.chars().all(|ch| "$#>%".contains(ch));
            let tail = if partial.is_empty() || bare_prompt { c.last.as_str() } else { partial };
            let tail = if tail.is_empty() { String::new() } else { format!(" · {}", clip(tail, 80)) };
            return (format!("⏵ {cmd} · {{dur}}{tail}"), c.start);
        }
        match self.last_cmd() {
            Some(c) => {
                let end = c.end.unwrap_or(c.start);
                let cmd = clip(&c.cmd, 60);
                match c.exit {
                    Some(e) if e != 0 => {
                        let err = c.errors.first().map(|e| format!(" · {}", clip(e, 80))).unwrap_or_default();
                        (format!("✗ {cmd} ({e}) · {{ago}}{err}"), end)
                    }
                    _ => (format!("✓ {cmd} · {{ago}}"), end),
                }
            }
            None => (idle(), 0),
        }
    }

    /// ` · <title>` for the status line, or nothing.
    fn title_part(&self) -> String {
        if self.title.is_empty() { String::new() } else { format!(" · {}", clip(&self.title, 60)) }
    }

    /// The `seq` a summary made now covers: everything before the oldest command still running (that one is
    /// listed again next time, with its result).
    pub fn covered_seq(&self) -> u64 {
        self.running_cmd().map_or(self.total, |c| c.seq.saturating_sub(1))
    }

    /// Did anything happen that a summary covering the first `seq` commands, made while a command was running
    /// (`running`: the one numbered `seq + 1`), has not seen? A later command, or that running command being over.
    pub fn activity_since(&self, seq: u64, running: bool) -> bool {
        let seen = seq + u64::from(running);
        self.entries.iter().any(|c| !c.cmd.is_empty() && (c.seq > seen || (running && c.seq == seen && c.end.is_some())))
    }
}

/// The git branch (or short commit id) of the repository containing `cwd`, if any. Handles worktrees (`.git` file).
pub fn branch_of(cwd: &Path) -> Option<String> {
    git_branch(&find_repo(cwd)?)
}

/// `cwd: ~/src/nt (git nt, branch reconnect)`
pub fn header(cwd: &Path, home: &str) -> String {
    let shown = tilde(&cwd.display().to_string(), home);
    match find_repo(cwd) {
        Some(r) => {
            let name = r.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
            match git_branch(&r) {
                Some(b) => format!("cwd: {shown} (git {name}, branch {b})"),
                None => format!("cwd: {shown} (git {name})"),
            }
        }
        None => format!("cwd: {shown}"),
    }
}

pub fn git_branch(repo: &Path) -> Option<String> {
    let dotgit = repo.join(".git");
    let gitdir = if dotgit.is_file() {
        let t = std::fs::read_to_string(&dotgit).ok()?;
        let p = PathBuf::from(t.trim().strip_prefix("gitdir:")?.trim());
        if p.is_absolute() { p } else { repo.join(p) }
    } else {
        dotgit
    };
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref: refs/heads/") {
        Some(b) => b.to_string(),
        None => clip(head, 7),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osc(s: &str) -> Option<ShellEv> {
        let parts: Vec<&[u8]> = s.split(';').map(str::as_bytes).collect();
        parse_osc(&parts)
    }

    #[test]
    fn osc_parsing() {
        assert_eq!(osc("133;A"), Some(ShellEv::Prompt));
        assert_eq!(osc("133;C"), Some(ShellEv::OutputStart));
        assert_eq!(osc("133;D;0"), Some(ShellEv::Done(Some(0))));
        assert_eq!(osc("133;D;101"), Some(ShellEv::Done(Some(101))));
        assert_eq!(osc("133;D"), Some(ShellEv::Done(None)));
        assert_eq!(osc("133;B"), None);
        assert_eq!(osc("8;;http://x"), None);
        assert_eq!(osc("133;E;make -j8"), Some(ShellEv::CmdLine("make -j8".into())));
        // raw ';' is rejoined, escapes are decoded, controls are neutralised
        assert_eq!(osc("133;E;a;b"), Some(ShellEv::CmdLine("a;b".into())));
        assert_eq!(osc("133;E;echo\\x3b ls\\x0arm\\x1b[2J\\x07x"), Some(ShellEv::CmdLine("echo; ls rm [2J x".into())));
        assert_eq!(osc("133;E;caf\\xc3\\xa9 \\\\x41"), Some(ShellEv::CmdLine("café \\x41".into())));
        assert_eq!(osc("7;file://host/home/a%20b/c"), Some(ShellEv::Cwd("/home/a b/c".into())));
        assert_eq!(osc("7;file:///tmp"), Some(ShellEv::Cwd("/tmp".into())));
        assert_eq!(osc("7;http://x/y"), None);
    }

    #[test]
    fn unescape_is_clean() {
        let u = unescape_cmd("\\x1b]0;evil\\x07 \u{1b}\u{7}\n\t ok \\xZZ trailing\\x");
        assert!(!u.chars().any(char::is_control), "{u:?}");
        assert!(u.ends_with("trailing\\x") && u.contains("\\xZZ"));
        assert_eq!(unescape_cmd(&"a".repeat(900)).len(), 500);
    }

    fn si<'a>() -> StatusIn<'a> {
        StatusIn { alt: false, fg_cmd: "", fg_is_shell: true, cwd: "/home/u/src/nt", home: "/home/u", partial: "", exited: false }
    }

    fn run(log: &mut CommandLog, cmd: &str, t0: u64, t1: u64, exit: i32, lines: &[&str]) {
        log.apply(ShellEv::CmdLine(cmd.into()), t0);
        log.apply(ShellEv::OutputStart, t0);
        for l in lines {
            log.line(l);
        }
        log.apply(ShellEv::Done(Some(exit)), t1);
        log.apply(ShellEv::Prompt, t1);
    }

    #[test]
    fn log_and_status() {
        let mut log = CommandLog::default();
        assert_eq!(log.status(&si()), ("idle · ~/src/nt".to_string(), 0));
        log.apply(ShellEv::Cwd("/home/u/src/nt".into()), 1);
        log.apply(ShellEv::Prompt, 1);
        assert!(log.integrated);
        assert_eq!(log.status(&si()).0, "idle · ~/src/nt");

        run(&mut log, "cargo build", 10, 22, 0, &["   Compiling nt", "    Finished"]);
        assert_eq!(log.status(&si()), ("✓ cargo build · {ago}".to_string(), 22));

        run(&mut log, "cargo test", 30, 70, 101, &["running 3 tests", "error[E0308]: mismatched types", "error: could not compile", "error: e3", "error: e4"]);
        let (s, since) = log.status(&si());
        assert_eq!((s.as_str(), since), ("✗ cargo test (101) · {ago} · error[E0308]: mismatched types", 70));
        assert_eq!(log.last_cmd().unwrap().errors.len(), 3);

        // empty prompt: no command
        log.apply(ShellEv::Done(Some(0)), 80);
        log.apply(ShellEv::Prompt, 80);
        assert_eq!(log.entries().len(), 2);

        log.apply(ShellEv::CmdLine("make deploy".into()), 100);
        log.apply(ShellEv::OutputStart, 100); // duplicate start (fish) is harmless
        assert!(log.is_running() && log.entries().len() == 3);
        log.line("Uploading 45%");
        assert_eq!(log.status(&si()), ("⏵ make deploy · {dur} · Uploading 45%".to_string(), 100));
        let st = StatusIn { partial: "Uploading 50%", ..si() };
        assert_eq!(log.status(&st).0, "⏵ make deploy · {dur} · Uploading 50%");
        let st = StatusIn { alt: true, fg_cmd: "vim", fg_is_shell: false, ..si() };
        assert_eq!(log.status(&st).0, "⏵ make deploy");
        log.apply(ShellEv::Done(Some(0)), 110);
        log.apply(ShellEv::Done(Some(5)), 111); // duplicate D ignored
        assert_eq!(log.last_cmd().unwrap().exit, Some(0));

        run(&mut log, "vim src/proto.rs", 120, 120, 0, &[]);
        log.apply(ShellEv::CmdLine("vim src/proto.rs".into()), 130);
        let st = StatusIn { alt: true, ..si() };
        assert_eq!(log.status(&st).0, "✎ vim src/proto.rs");
        assert_eq!(log.status(&StatusIn { exited: true, ..si() }).0, "exited");
    }

    #[test]
    fn log_is_capped_and_no_hooks_fallback() {
        let mut log = CommandLog::default();
        for i in 0..60 {
            run(&mut log, &format!("c{i}"), i, i + 1, 0, &[]);
        }
        assert_eq!(log.entries().len(), 50);
        assert_eq!(log.entries().front().unwrap().cmd, "c10");
        let plain = CommandLog::default();
        let st = StatusIn { fg_cmd: "htop", fg_is_shell: false, alt: true, ..si() };
        assert_eq!(plain.status(&st).0, "⏵ htop");
        let st = StatusIn { fg_cmd: "vim", fg_is_shell: false, alt: true, ..si() };
        assert_eq!(plain.status(&st).0, "✎ vim");
    }

    #[test]
    fn summary_staleness_follows_commands() {
        let mut log = CommandLog::default();
        run(&mut log, "ls", 0, 1, 0, &["a"]);
        // a summary made while `sleep 300` runs covers `ls` (seq 1) and saw the build running (seq 2)
        log.apply(ShellEv::CmdLine("sleep 300".into()), 10);
        log.apply(ShellEv::OutputStart, 10);
        let (seq, running) = (log.covered_seq(), log.is_running());
        assert_eq!((seq, running), (1, true));
        assert!(!log.activity_since(seq, running), "the summary saw it running");
        // Ctrl-C: the shell reports the end with 130
        log.apply(ShellEv::Done(Some(130)), 20);
        assert!(log.activity_since(seq, running));
        // a summary made now covers it; nothing newer
        let (seq, running) = (log.covered_seq(), log.is_running());
        assert!(!log.activity_since(seq, running));
        run(&mut log, "make", 30, 31, 0, &["ok"]);
        assert!(log.activity_since(seq, running));
    }

    #[test]
    fn last_command_line_for_the_model() {
        let mut log = CommandLog::default();
        assert_eq!(log.last_command_line(), "");
        log.apply(ShellEv::Cwd("/home/u/src/nt".into()), 0);
        run(&mut log, "cargo build", 0, 12, 0, &["ok"]);
        assert_eq!(log.last_command_line(), "last command: cargo build · ok");
        run(&mut log, "cargo test", 20, 60, 101, &["error[E0308]: mismatched types"]);
        assert_eq!(log.last_command_line(), "last command: cargo test · exit 101");
        log.apply(ShellEv::CmdLine("make deploy".into()), 100);
        log.apply(ShellEv::OutputStart, 100);
        assert_eq!(log.last_command_line(), "running command: make deploy");
        // A summary made now covers the two finished commands; the running one is listed again next time.
        assert_eq!((log.total(), log.covered_seq()), (3, 2));
    }

    #[test]
    fn git_header() {
        let d = std::env::temp_dir().join(format!("nt-hdr-{}", std::process::id()));
        std::fs::create_dir_all(d.join("r/.git")).unwrap();
        std::fs::create_dir_all(d.join("r/sub")).unwrap();
        std::fs::write(d.join("r/.git/HEAD"), "ref: refs/heads/reconnect\n").unwrap();
        assert_eq!(header(&d.join("r/sub"), "/nonexistent"), format!("cwd: {} (git r, branch reconnect)", d.join("r/sub").display()));
        assert_eq!(header(&d, &d.display().to_string()), "cwd: ~");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn title_osc_events() {
        assert_eq!(osc("0;✳ Fix bug"), Some(ShellEv::Title("✳ Fix bug".into())));
        assert_eq!(osc("2;a;b"), Some(ShellEv::Title("a;b".into())));
        assert_eq!(osc("0;\u{1}x\u{7f}y  "), Some(ShellEv::Title("xy".into())));
        let long = format!("0;{}", "é".repeat(300));
        assert!(matches!(osc(&long), Some(ShellEv::Title(t)) if t.chars().count() == MAX_TITLE));
    }

    #[test]
    fn default_titles_are_ignored() {
        let h = "/home/u";
        assert!(is_default_title("u@box: ~/src/nt", "/home/u/src/nt", h, ""));
        assert!(is_default_title("u@box:/tmp", "", h, ""));
        assert!(is_default_title("~/src/nt", "/home/u/src/nt", h, ""));
        assert!(is_default_title("/home/u/src/nt", "/home/u/src/nt", h, ""));
        assert!(is_default_title("vim a.rs", "", h, "vim a.rs"));
        assert!(is_default_title("claude", "", h, "claude --resume"));
        assert!(!is_default_title("✳ Fix reconnect bug", "/home/u", h, "claude"));
        assert!(!is_default_title("Note: remember this", "", h, ""));
        let mut log = CommandLog::default();
        log.apply(ShellEv::Prompt, 1);
        // between D and the next command: the shell's own title
        assert!(!log.note_title("✳ Fix reconnect bug", None, h));
        assert!(log.title.is_empty());
        log.apply(ShellEv::CmdLine("claude".into()), 3);
        log.apply(ShellEv::OutputStart, 3);
        assert!(!log.note_title("u@box: ~", None, h));
        assert!(log.note_title("✳ Fix reconnect bug", None, h));
        assert_eq!(log.title, "Fix reconnect bug");
        log.apply(ShellEv::Done(Some(0)), 5);
        assert!(log.title.is_empty());
        // no integration: ignored while the shell is foreground
        let mut bare = CommandLog::default();
        assert!(!bare.note_title("u@box: ~", Some(true), h));
        assert!(!bare.note_title("Fix it", Some(true), h));
        assert!(bare.note_title("Fix it", Some(false), h));
    }

    #[test]
    fn spinner_churn_is_not_a_title_change() {
        let mut log = CommandLog::default();
        log.apply(ShellEv::CmdLine("claude".into()), 100);
        log.apply(ShellEv::OutputStart, 100);
        assert!(log.note_title("✳ Fix reconnect bug", None, ""));
        for g in ["⠂", "⠐", "✳", "·", "*"] {
            assert!(!log.note_title(&format!("{g} Fix reconnect bug"), None, ""));
        }
        assert!(log.note_title("✳ Add tests", None, ""));
        assert_eq!(log.title, "Add tests");
        assert_eq!(title_text("/usr/bin x"), "/usr/bin x");
        assert_eq!(title_text("⠂ x y"), "x y");
    }

    #[test]
    fn status_with_title() {
        let mut log = CommandLog::default();
        log.apply(ShellEv::Cwd("/home/u/src/nt".into()), 1);
        log.apply(ShellEv::CmdLine("claude".into()), 100);
        log.apply(ShellEv::OutputStart, 100);
        log.note_title("Explore repo", None, "/home/u");
        log.note_title("✳ Fix reconnect bug in proto.rs", None, "/home/u");
        log.line("some redrawn line");
        assert_eq!(log.status(&si()), ("⏵ claude · Fix reconnect bug in proto.rs · {dur}".to_string(), 100));
        let st = StatusIn { alt: true, fg_cmd: "claude", fg_is_shell: false, ..si() };
        assert_eq!(log.status(&st).0, "⏵ claude · Fix reconnect bug in proto.rs");
        // vim
        let mut v = CommandLog::default();
        v.apply(ShellEv::CmdLine("vim README.md".into()), 1);
        v.apply(ShellEv::OutputStart, 1);
        v.note_title("README.md", None, "");
        let st = StatusIn { alt: true, fg_cmd: "vim", fg_is_shell: false, ..si() };
        assert_eq!(v.status(&st).0, "✎ vim README.md · README.md");
        // without hooks
        let mut b = CommandLog::default();
        b.note_title("Fix it", Some(false), "");
        let st = StatusIn { fg_cmd: "claude", fg_is_shell: false, ..si() };
        assert_eq!(b.status(&st).0, "⏵ claude · Fix it");
    }

    #[test]
    fn nested_shell_prompt_is_not_the_status() {
        let mut log = CommandLog::default();
        log.apply(ShellEv::CmdLine("cd src && sh".into()), 100);
        log.apply(ShellEv::OutputStart, 100);
        log.line("build ok");
        assert_eq!(log.status(&StatusIn { partial: "$ ", ..si() }).0, "⏵ cd src && sh · {dur} · build ok");
        assert_eq!(log.status(&StatusIn { partial: "Password: ", ..si() }).0, "⏵ cd src && sh · {dur} · Password:");
    }
}
