//! Session names and recaps are made by a short-lived child process (`nt namer`), never inside the daemon.
//! This module has the line protocol shared by both sides, the sanitizers, the daemon-side batch runner
//! and the child's low-priority setup. The local model lives in `llm.rs` (cargo feature `llm`), the API backend in `api.rs`, the prompt in `prompt.rs`.
//!
//! Protocol: one job per stdin line, one result per stdout line; fields are tab separated with
//! `\\`, `\t`, `\n` and `\r` escaped. Job: `sid digest`. Result: `sid name topic summary` (an empty field = no usable
//! value; the topic is stored as the session's `recap`).

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::config::{Config, LlmMode};
use crate::proto::SessionId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub sid: SessionId,
    /// The model input (see `shell::CommandLog::digest` / `guess_digest`).
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Out {
    pub sid: SessionId,
    pub name: Option<String>,
    pub recap: Option<String>,
    pub summary: Option<String>,
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\t' => o.push_str("\\t"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            c => o.push(c),
        }
    }
    o
}

fn unesc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            o.push(c);
            continue;
        }
        match it.next() {
            Some('t') => o.push('\t'),
            Some('n') => o.push('\n'),
            Some('r') => o.push('\r'),
            Some(c) => o.push(c),
            None => {}
        }
    }
    o
}

impl Job {
    pub fn to_line(&self) -> String {
        format!("{}\t{}", self.sid, esc(&self.text))
    }

    pub fn from_line(line: &str) -> Option<Job> {
        let (sid, text) = line.split_once('\t')?;
        Some(Job { sid: sid.parse().ok()?, text: unesc(text) })
    }
}

impl Out {
    pub fn to_line(&self) -> String {
        let f = |v: &Option<String>| esc(v.as_deref().unwrap_or(""));
        format!("{}\t{}\t{}\t{}", self.sid, f(&self.name), f(&self.recap), f(&self.summary))
    }

    /// Parses and re-sanitizes (the daemon does not trust the child's output blindly).
    pub fn from_line(line: &str) -> Option<Out> {
        let mut it = line.split('\t');
        let sid = it.next()?.parse().ok()?;
        let name = it.next().map(unesc).and_then(|s| sanitize_name(&s));
        let recap = it.next().map(unesc).and_then(|s| sanitize_recap(&s));
        let summary = it.next().map(unesc).and_then(|s| sanitize_summary(&s));
        Some(Out { sid, name, recap, summary })
    }
}

/// Model output -> 2..=4 lowercase kebab-case words, or None if it is unusable
/// (too many words, a repeated word, 'terminal session' filler).
pub fn sanitize_name(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let kebab: String = line.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let words: Vec<&str> = kebab.split('-').filter(|w| !w.is_empty()).collect();
    let repeated = words.iter().enumerate().any(|(i, w)| words[..i].contains(w));
    if !(2..=4).contains(&words.len()) || words.iter().any(|w| w.len() > 24) || repeated || kebab.contains("terminal-session") {
        return None;
    }
    Some(words.join("-"))
}

/// Longest recap kept.
pub const RECAP_MAX: usize = 50;

/// Model output -> one clean line of at most `RECAP_MAX` chars, or None if it is unusable.
pub fn sanitize_recap(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let clean: String = line.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    // First sentence only, without the model's "The user is" filler.
    let clean = clean.split_once(". ").map_or(clean.as_str(), |(first, _)| first);
    let lower = clean.to_lowercase();
    let clean = ["the user is ", "the user was ", "user is ", "the user "]
        .iter()
        .find(|p| lower.starts_with(**p))
        .map_or(clean, |p| &clean[p.len()..]);
    let words: Vec<&str> = clean.split_whitespace().collect();
    let joined = words.join(" ");
    let joined = joined.trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '*').trim();
    let mut out: String = joined.chars().take(RECAP_MAX).collect();
    if joined.chars().count() > RECAP_MAX {
        // Don't end in the middle of a word.
        if let Some(i) = out.rfind(' ') {
            out.truncate(i);
        }
    }
    let out = out.trim_end_matches(['.', ',', ';', ':', ' ']).to_string();
    let low = out.to_lowercase();
    let words: Vec<&str> = low.split_whitespace().collect();
    let looping = words.windows(2).filter(|w| w[0] == w[1]).count() >= 2;
    if out.chars().count() < 8 || !out.contains(' ') || low.contains("terminal session") || looping {
        return None;
    }
    Some(out)
}

/// Longest summary kept.
pub const SUMMARY_MAX: usize = 300;

/// Model output -> one clean paragraph of at most `SUMMARY_MAX` chars (cut after the last complete sentence
/// that fits, else at a word), or None if it is unusable.
pub fn sanitize_summary(raw: &str) -> Option<String> {
    let flat: String = raw.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let words: Vec<&str> = flat.split_whitespace().collect();
    let joined = words.join(" ");
    let mut out = joined.trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '*').trim().to_string();
    if out.chars().count() > SUMMARY_MAX {
        let cut: String = out.chars().take(SUMMARY_MAX).collect();
        // Prefer the end of the last complete sentence if that keeps most of the text.
        let sentence_end = cut.rmatch_indices(['.', '!', '?']).map(|(i, _)| i + 1).find(|&i| cut[i..].is_empty() || cut[i..].starts_with(' '));
        out = match sentence_end {
            Some(i) if cut[..i].chars().count() >= SUMMARY_MAX / 2 => cut[..i].to_string(),
            _ => cut.rfind(' ').map_or(cut.clone(), |i| cut[..i].to_string()),
        };
    }
    let out = out.trim_end_matches([',', ';', ':', ' ']).to_string();
    let out = if out.ends_with(['.', '!', '?']) { out } else { out + "." };
    let low = out.to_lowercase();
    let ws: Vec<&str> = low.split_whitespace().collect();
    let looping = ws.windows(2).filter(|w| w[0] == w[1]).count() >= 3;
    if out.chars().count() < 20 || ws.len() < 4 || looping {
        return None;
    }
    Some(out)
}

/// Phrases that state what is going on right now; the status line shows live state, a summary is history.
const LIVE_PHRASES: &[&str] = &["currently", "is running", "are running", "still running", "in progress", "has been running", "have been running", "is building", "is compiling", "is still "];

/// `for 5 minutes`, `for about 10 min`, `for 2h`: an elapsed time that is only true while something runs.
fn has_elapsed(low: &str) -> bool {
    const UNITS: &[&str] = &["s", "m", "h", "sec", "secs", "second", "seconds", "min", "mins", "minute", "minutes", "hr", "hrs", "hour", "hours"];
    const FILLER: &[&str] = &["about", "over", "nearly", "almost", "around", "more", "than", "the", "last", "roughly"];
    let w: Vec<&str> = low.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')')).filter(|x| !x.is_empty()).collect();
    let word = |i: usize| w.get(i).map_or("", |t| t.trim_end_matches(['.', '!', '?']));
    (0..w.len()).filter(|&i| w[i] == "for").any(|i| {
        let mut j = i + 1;
        while FILLER.contains(&word(j)) {
            j += 1;
        }
        let t = word(j);
        let rest = t.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
        let has_num = rest.len() < t.len();
        has_num && ((rest.is_empty() && UNITS.contains(&word(j + 1))) || UNITS.contains(&rest))
    })
}

/// Drop the sentences of a summary that claim live state ("currently building", "is still running", "for 5 minutes",
/// "in progress"). `running`: a command is running right now, then the summary is left alone.
/// `None` if nothing usable is left.
pub fn drop_live_state(summary: &str, running: bool) -> Option<String> {
    if running {
        return Some(summary.to_string());
    }
    let mut sentences: Vec<&str> = Vec::new();
    let mut start = 0;
    let b = summary.as_bytes();
    for i in 0..b.len() {
        if matches!(b[i], b'.' | b'!' | b'?') && b.get(i + 1).is_none_or(|c| *c == b' ') {
            sentences.push(summary[start..=i].trim());
            start = i + 1;
        }
    }
    if start < summary.len() {
        sentences.push(summary[start..].trim());
    }
    let kept: Vec<&str> = sentences
        .into_iter()
        .filter(|x| !x.is_empty())
        .filter(|x| {
            let low = x.to_lowercase();
            !LIVE_PHRASES.iter().any(|p| low.contains(p)) && !has_elapsed(&low)
        })
        .collect();
    sanitize_summary(&kept.join(" "))
}

// ---------------------------------------------------------------- digest

/// The command typed on a prompt-looking line (`user@host:~$ ls`, `$ ls`, `% ls`, `# ls`, `> ls`).
fn prompt_command(line: &str) -> Option<&str> {
    let l = line.trim_end();
    let (at, _) = ["$ ", "% ", "# ", "> "]
        .iter()
        .filter_map(|m| l.find(m).map(|i| (i, m)))
        .filter(|&(i, m)| if i == 0 { matches!(*m, "$ " | "% ") } else { i <= 80 && !l[..i].ends_with([' ', '-', '=', '|']) })
        .min_by_key(|&(i, _)| i)?;
    let cmd = l[at + 2..].trim();
    (!cmd.is_empty()).then_some(cmd)
}

/// The digest for a session without shell integration: commands guessed from prompt-looking transcript
/// lines (exit status unknown), the last few error-looking lines under the final one.
pub fn guess_digest(header: &str, text: &str, fg: &str, prev: Option<&str>) -> String {
    let clip = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    let mut cmds: Vec<String> = Vec::new();
    let mut errs: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(c) = prompt_command(line) {
            let c = clip(c, 80);
            if cmds.last() != Some(&c) {
                cmds.push(c);
            }
        } else {
            let low = line.to_lowercase();
            if super::shell::ERROR_WORDS.iter().any(|w| low.contains(w)) {
                let e = clip(line.trim(), 100);
                if !errs.contains(&e) {
                    errs.push(e);
                }
            }
        }
    }
    let tail = |v: &mut Vec<String>, n: usize| v.drain(..v.len().saturating_sub(n)).for_each(drop);
    tail(&mut cmds, 8);
    tail(&mut errs, 3);
    let mut out = String::new();
    if let Some(p) = prev {
        out.push_str(&format!("earlier summary: {}\n", p.trim()));
    }
    out.push_str(&format!("{header}\n{}\n", if prev.is_some() { "new commands since then (oldest first):" } else { "commands (oldest first):" }));
    if cmds.is_empty() && errs.is_empty() {
        // Nothing recognisable: the last few lines of output.
        let mut last: Vec<String> = text.lines().filter(|l| !l.trim().is_empty()).map(|l| clip(l.trim(), 100)).collect();
        tail(&mut last, 5);
        last.iter().for_each(|l| out.push_str(&format!("      | {l}\n")));
    }
    let n = cmds.len();
    for (i, c) in cmds.iter().enumerate() {
        out.push_str(&format!("  {c}\n"));
        if i + 1 == n {
            errs.iter().for_each(|e| out.push_str(&format!("      | {e}\n")));
        }
    }
    if cmds.is_empty() {
        errs.iter().for_each(|e| out.push_str(&format!("      | {e}\n")));
    }
    if !fg.is_empty() {
        out.push_str(&format!("foreground: {fg}\n"));
    }
    out
}

// ---------------------------------------------------------------- scheduling

use std::time::Duration;

/// At most one background batch per this long (measured from the start of the last one).
pub const BATCH_EVERY: Duration = Duration::from_secs(120);
/// A session must have been silent this long before a background batch picks it up.
pub const QUIET_FOR: Duration = Duration::from_secs(20);

/// `ssh host ...`, also with a path or `sudo`-less prefix like `/usr/bin/ssh`.
pub fn is_ssh(cmd: &str) -> bool {
    cmd.split_whitespace().next().is_some_and(|w| w.rsplit('/').next() == Some("ssh"))
}

/// May a batch start now? `since_start`: time since the last batch started (None = never).
/// `force` is the switcher-open request: no cooldown (the resource guards still apply).
pub fn batch_allowed(force: bool, since_start: Option<Duration>) -> bool {
    force || since_start.is_none_or(|d| d >= BATCH_EVERY)
}

/// A session that could get a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cand {
    pub sid: SessionId,
    /// New transcript bytes.
    pub new: u64,
    pub last_used: u64,
}

/// Job order. Opened switcher (`watcher_current` = Some(the watching client's current session, if any)): that session
/// first, then the others by last use, the order the switcher lists them in. Background: most new output first.
pub fn order_cands(c: &mut [Cand], watcher_current: Option<Option<SessionId>>) {
    match watcher_current {
        Some(cur) => c.sort_by_key(|x| (Some(x.sid) != cur, std::cmp::Reverse(x.last_used), x.sid)),
        None => c.sort_by_key(|x| (std::cmp::Reverse(x.new), x.sid)),
    }
}

/// Is this dirty session ready for a batch? Background batches wait until it has been quiet for `QUIET_FOR`.
pub fn session_ready(dirty: bool, quiet: Duration, force: bool) -> bool {
    dirty && (force || quiet >= QUIET_FOR)
}

// ---------------------------------------------------------------- daemon side

/// The program to run as the namer child: our own image. On Linux `/proc/self/exe` works even when the
/// binary on disk was replaced or deleted after the daemon started (an upgrade), unlike `current_exe()`'s path.
fn namer_exe() -> Result<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    if std::path::Path::new("/proc/self/exe").exists() {
        return Ok("/proc/self/exe".into());
    }
    std::env::current_exe().context("current_exe")
}

/// Run one `nt namer` child over `jobs`. Blocking: call it from `spawn_blocking`.
/// Each result is handed to `on_out` as soon as the child prints it.
pub fn run_batch(cfg: &Config, jobs: &[Job], mut on_out: impl FnMut(Out)) -> Result<()> {
    let local = cfg.llm == LlmMode::Local;
    let cap = super::guard::estimate(super::guard::model_size(&cfg.llm_model));
    let mut cmd = match local.then(|| scope_cap(cap)).flatten() {
        // A cgroup scope with a hard memory limit and the lowest CPU weight; the child execs in place.
        Some(mem_max) => {
            let mut c = Command::new("systemd-run");
            c.args(["--user", "--scope", "--quiet", "-p"])
                .arg(format!("MemoryMax={mem_max}"))
                .args(["-p", "MemorySwapMax=0", "-p", "CPUWeight=1", "--"])
                // Our own pid path, not /proc/self/exe: systemd-run resolves it in its own process.
                .arg(format!("/proc/{}/exe", std::process::id()));
            c.env("NT_NAMER_CAPPED", "1");
            c
        }
        None => Command::new(namer_exe()?),
    };
    cmd.arg("namer").arg("--model").arg(&cfg.llm_model);
    cmd.env("NT_NAMER_MEMCAP", cap.to_string());
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("spawning nt namer")?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let input: String = jobs.iter().map(|j| j.to_line() + "\n").collect();
    // The child reads all jobs before answering, so a single write cannot deadlock on its output.
    let w = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let watch = Watchdog::start(child.id());
    let mut n = 0;
    for line in BufReader::new(child.stdout.take().context("stdout")?).lines() {
        if let Some(o) = Out::from_line(&line?) {
            n += 1;
            on_out(o);
        }
    }
    let _ = w.join();
    if let Some(reason) = watch.stop() {
        let _ = child.wait();
        bail!("nt namer killed: {reason}");
    }
    let status = child.wait()?;
    if !status.success() && n == 0 {
        bail!("nt namer failed ({status})");
    }
    Ok(())
}

/// Kills the namer child when memory runs short or it takes too long; polls once a second.
struct Watchdog {
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: std::thread::JoinHandle<Option<String>>,
}

impl Watchdog {
    fn start(pid: u32) -> Watchdog {
        use std::sync::atomic::{AtomicBool, Ordering};
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let thread = std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            let mut last = t0;
            while !flag.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(100));
                if last.elapsed() < Duration::from_secs(1) {
                    continue;
                }
                last = std::time::Instant::now();
                if let Some(reason) = super::guard::must_kill(&super::guard::read_meminfo(), t0.elapsed()) {
                    eprintln!("nt: killing the namer: {reason}");
                    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL);
                    return Some(reason);
                }
            }
            None
        });
        Watchdog { done, thread }
    }

    /// Stop watching; `Some(reason)` if the child was killed.
    fn stop(self) -> Option<String> {
        self.done.store(true, std::sync::atomic::Ordering::SeqCst);
        self.thread.join().ok().flatten()
    }
}

/// `MemoryMax` for a `systemd-run --user --scope` wrapper, if the user manager can do that here (probed once).
fn scope_cap(estimate: u64) -> Option<u64> {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let ok = *OK.get_or_init(|| {
        Command::new("systemd-run")
            .args(["--user", "--scope", "--quiet", "-p", "MemoryMax=8G", "-p", "CPUWeight=1", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    });
    ok.then_some(estimate + 256 * (1 << 20))
}

// ---------------------------------------------------------------- child side

/// First to go when the kernel is out of memory.
fn make_oom_victim() {
    let _ = std::fs::write("/proc/self/oom_score_adj", "1000");
}

/// Without a cgroup scope: cap the heap and private mappings (the mmap'd weights are file-backed and not counted).
fn limit_data(bytes: u64) {
    let lim = nix::libc::rlimit { rlim_cur: bytes, rlim_max: bytes };
    // SAFETY: valid pointer to a rlimit struct.
    unsafe { nix::libc::setrlimit(nix::libc::RLIMIT_DATA, &lim) };
}

/// Be as unobtrusive as possible: idle CPU scheduling class, nice 19, idle I/O class.
pub fn lower_priority() {
    // SAFETY: plain syscalls with valid arguments; run before any worker thread exists, so the
    // settings are inherited by every thread the child starts later.
    unsafe {
        #[cfg(target_os = "linux")]
        {
            let sp = nix::libc::sched_param { sched_priority: 0 };
            nix::libc::sched_setscheduler(0, nix::libc::SCHED_IDLE, &sp);
            // ioprio_set(IOPRIO_WHO_PROCESS, 0, IOPRIO_CLASS_IDLE << 13)
            nix::libc::syscall(nix::libc::SYS_ioprio_set, 1, 0, 3 << 13);
        }
        nix::libc::setpriority(nix::libc::PRIO_PROCESS, 0, 19);
    }
}

/// `nt namer`: jobs on stdin (until EOF), results on stdout, then exit.
pub fn namer_main(cfg: &Config) -> Result<()> {
    lower_priority();
    make_oom_victim();
    if cfg.llm == LlmMode::Local
        && std::env::var_os("NT_NAMER_CAPPED").is_none()
        && let Some(cap) = std::env::var("NT_NAMER_MEMCAP").ok().and_then(|v| v.parse::<u64>().ok())
    {
        limit_data(cap + 512 * (1 << 20));
    }
    // Watchdog: never outlive a stuck download or inference by much.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(20 * 60));
        std::process::exit(2);
    });
    let jobs: Vec<Job> = std::io::stdin().lock().lines().map_while(Result::ok).filter_map(|l| Job::from_line(&l)).collect();
    run_jobs(cfg, &jobs)
}

fn run_jobs(cfg: &Config, jobs: &[Job]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    let mut emit = |j: &Job, r: super::api::Answer| {
        let out = match r {
            Ok(r) => Out { sid: j.sid, name: r.name, recap: r.topic, summary: r.summary },
            Err(e) => {
                eprintln!("nt namer: job {}: {e:#}", j.sid);
                Out { sid: j.sid, name: None, recap: None, summary: None }
            }
        };
        let _ = writeln!(stdout, "{}", out.to_line()).and_then(|()| stdout.flush());
    };
    match cfg.llm {
        LlmMode::Api => super::api::run(cfg, jobs, emit),
        LlmMode::Local => run_local(cfg, jobs, &mut emit),
        LlmMode::Off => bail!("llm is off"),
    }
}

#[cfg(feature = "llm")]
fn run_local(cfg: &Config, jobs: &[Job], emit: &mut dyn FnMut(&Job, super::api::Answer)) -> Result<()> {
    super::llm::run(cfg, jobs, |_| {}, emit)
}

#[cfg(not(feature = "llm"))]
fn run_local(_: &Config, _: &[Job], _: &mut dyn FnMut(&Job, super::api::Answer)) -> Result<()> {
    bail!("built without the `llm` feature")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduling_rules() {
        let s = Duration::from_secs;
        // background: one batch per 2 min since the last start
        assert!(batch_allowed(false, None));
        assert!(!batch_allowed(false, Some(s(119))));
        assert!(batch_allowed(false, Some(s(120))));
        // switcher open: no cooldown
        assert!(batch_allowed(true, Some(s(1))));
        assert!(batch_allowed(true, None));
        // sessions
        assert!(!session_ready(false, s(999), true));
        assert!(!session_ready(true, s(19), false));
        assert!(session_ready(true, s(20), false));
        assert!(session_ready(true, s(0), true));
        assert!(is_ssh("ssh -p 22 host") && is_ssh("/usr/bin/ssh h") && !is_ssh("sshfs a b") && !is_ssh("echo ssh"));
    }

    #[test]
    fn live_state_is_dropped_from_summaries_of_idle_sessions() {
        let s = "Building the nt crate in ~/src/nt. The build is currently running for 5 minutes. Cargo reported a warning in proto.rs.";
        assert_eq!(drop_live_state(s, false).as_deref(), Some("Building the nt crate in ~/src/nt. Cargo reported a warning in proto.rs."));
        // a running command keeps its summary as it is
        assert_eq!(drop_live_state(s, true).as_deref(), Some(s));
        for live in [
            "The deploy is still running.",
            "The migration is in progress at 40%.",
            "Compiling for about 10 minutes.",
            "The build has been running for 2h.",
            "It is building the project.",
        ] {
            let text = format!("Worked on the billing service in ~/work/billing and ran the tests. {live}");
            assert_eq!(drop_live_state(&text, false).as_deref(), Some("Worked on the billing service in ~/work/billing and ran the tests."), "{live}");
        }
        // history stays: past tense, `after N minutes`, failures that "still" happen
        let ok = "Ran the build, which failed after 5 minutes with a linker error. Retrying still fails to link.";
        assert_eq!(drop_live_state(ok, false).as_deref(), Some(ok));
        // nothing usable left
        assert_eq!(drop_live_state("The build is currently running.", false), None);
    }

    #[test]
    fn job_order() {
        let c = |sid, new, last_used| Cand { sid, new, last_used };
        let ids = |v: &[Cand]| v.iter().map(|x| x.sid).collect::<Vec<_>>();
        let mut v = vec![c(1, 500, 10), c(2, 9000, 30), c(3, 100, 20), c(4, 100, 20)];
        order_cands(&mut v, Some(Some(1)));
        assert_eq!(ids(&v), [1, 2, 3, 4]); // current, then last use (ties by id)
        order_cands(&mut v, Some(Some(3)));
        assert_eq!(ids(&v), [3, 2, 4, 1]);
        order_cands(&mut v, Some(None));
        assert_eq!(ids(&v), [2, 3, 4, 1]); // no current session: last use only
        order_cands(&mut v, None);
        assert_eq!(ids(&v), [2, 1, 3, 4]); // background: most new output first
    }

    #[test]
    fn line_roundtrip() {
        let j = Job { sid: 7, text: "line1\nline2 \\ back\r\n\ttab".into() };
        let l = j.to_line();
        assert!(!l.contains('\n') && l.matches('\t').count() == 1);
        assert_eq!(Job::from_line(&l), Some(j));
        assert_eq!(Job::from_line("xy"), None);
        let o = Out {
            sid: 3,
            name: Some("fix-build".into()),
            recap: Some("fixing the build; tests fail".into()),
            summary: Some("Fixed a type error in proto.rs.\nTests pass now.".replace('\n', " ")),
        };
        assert_eq!(Out::from_line(&o.to_line()), Some(o));
        assert_eq!(Out::from_line("4\t\t"), Some(Out { sid: 4, name: None, recap: None, summary: None }));
        // an old three-field line still parses
        assert_eq!(Out::from_line("5\tab-cd\tfixing the build now").unwrap().summary, None);
    }

    #[test]
    fn guessed_digest() {
        let t = "user@host:~/nt$ cargo build\n   Compiling nt\nerror[E0308]: mismatched types\nerror: could not compile `nt`\n\
user@host:~/nt$ vim src/proto.rs\n$ cargo build\n% git status\n# comment line in output\n> quoted\nFAILED: permission denied\n";
        let d = guess_digest("cwd: ~/nt", t, "", None);
        assert_eq!(
            d,
            "cwd: ~/nt\ncommands (oldest first):\n  cargo build\n  vim src/proto.rs\n  cargo build\n  git status\n      | error[E0308]: mismatched types\n      | error: could not compile `nt`\n      | FAILED: permission denied\n"
        );
        assert!(guess_digest("cwd: /", "a\n\nb\n", "top", None).ends_with("      | a\n      | b\nforeground: top\n"));
        // rolling: the previous summary first, "new commands since then"
        assert_eq!(
            guess_digest("cwd: /", "$ ls\n", "", Some("Did x. ")),
            "earlier summary: Did x.\ncwd: /\nnew commands since then (oldest first):\n  ls\n"
        );
        assert_eq!(prompt_command("#hash"), None);
        assert_eq!(prompt_command("root@h:/# ls -l"), Some("ls -l"));
    }

    #[test]
    fn sanitizing_names() {
        assert_eq!(sanitize_name("Fix Login Bug\nmore").unwrap(), "fix-login-bug");
        assert_eq!(sanitize_name("\"rust_build errors.\"").unwrap(), "rust-build-errors");
        assert_eq!(sanitize_name("one two three four five six"), None);
        assert_eq!(sanitize_name("build-and-build"), None);
        assert_eq!(sanitize_name("terminal-session-help"), None);
        assert_eq!(sanitize_name("single"), None);
        assert_eq!(sanitize_name("   \n"), None);
    }

    #[test]
    fn sanitizing_summaries() {
        let ok = "Working on the nt reconnect logic in ~/src/nt. Fixed a type error in proto.rs after a failed cargo test; tests now pass.";
        assert_eq!(sanitize_summary(&format!("  \"{ok}\"\n")).unwrap(), ok);
        assert_eq!(sanitize_summary("Built the crate\x1b[0m in ~/src/nt\n and ran tests").unwrap(), "Built the crate [0m in ~/src/nt and ran tests.");
        assert_eq!(sanitize_summary("ok"), None);
        assert_eq!(sanitize_summary(""), None);
        assert_eq!(sanitize_summary("done done done done done done done"), None);
        // too long: cut after the last complete sentence that fits
        let long = format!("{} Second sentence is here and it is quite long as well, so it has to go. {}", "First sentence is about the nt project and its tests. ".repeat(4).trim(), "tail ".repeat(30));
        let r = sanitize_summary(&long).unwrap();
        assert!(r.chars().count() <= SUMMARY_MAX && r.ends_with("go."), "{r}");
        // no sentence boundary: cut at a word
        let words = (0..80).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
        let r = sanitize_summary(&words).unwrap();
        assert!(r.chars().count() <= SUMMARY_MAX + 1 && !r.ends_with(char::is_numeric) && r.ends_with('.'), "{r}");
    }

    #[test]
    fn sanitizing_recaps() {
        assert_eq!(sanitize_recap("  \"Debugging tcp reconnect in nt; tests pass.\"  ").unwrap(), "Debugging tcp reconnect in nt; tests pass");
        assert_eq!(sanitize_recap("a\x1b[31mb\x07 c dd eee").unwrap(), "a [31mb c dd eee");
        assert_eq!(sanitize_recap("The user is updating the script. Then more text.").unwrap(), "updating the script");
        assert_eq!(sanitize_recap("short"), None);
        assert_eq!(sanitize_recap(""), None);
        let long = (0..40).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
        let r = sanitize_recap(&long).unwrap();
        assert!(r.chars().count() <= RECAP_MAX && r.ends_with(char::is_numeric));
        assert_eq!(sanitize_recap("The terminal session is being built"), None);
    }
}
