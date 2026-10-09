//! Session names: random initial `adjective-noun`, then heuristic `repo:cmd` names from the foreground process.

use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use crate::proto::NameSource;

const ADJECTIVES: &[&str] = &[
    "amber", "brave", "calm", "clever", "cosmic", "crisp", "daring", "eager", "fancy", "gentle",
    "golden", "happy", "icy", "jolly", "keen", "lively", "lucky", "mellow", "nimble", "odd",
    "plucky", "quiet", "rapid", "shiny", "silent", "sunny", "swift", "tidy", "vivid", "witty",
];

const NOUNS: &[&str] = &[
    "badger", "beacon", "comet", "cricket", "delta", "ember", "falcon", "fjord", "gecko", "harbor",
    "heron", "island", "juniper", "kettle", "lantern", "lynx", "maple", "meadow", "otter",
    "pebble", "quartz", "raven", "river", "sparrow", "summit", "thistle", "tundra", "walrus",
    "willow", "zephyr",
];

fn pick(list: &[&'static str]) -> &'static str {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).expect("getrandom");
    list[u32::from_le_bytes(b) as usize % list.len()]
}

/// A name not present in `taken` (falls back to a numeric suffix if unlucky).
pub fn initial_name(taken: &[&str]) -> String {
    for _ in 0..20 {
        let name = format!("{}-{}", pick(ADJECTIVES), pick(NOUNS));
        if !taken.contains(&name.as_str()) {
            return name;
        }
    }
    format!("{}-{}-{}", pick(ADJECTIVES), pick(NOUNS), taken.len())
}

/// Heuristic naming period.
pub const INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
/// A session counts as having recent output if it was active within this many seconds.
pub const RECENT_SECS: u64 = 60;

/// What the heuristic learned about a session's foreground state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// Repo (or directory) name: the "context" whose change may override an LLM name.
    pub ctx: String,
    /// Short command label (`vim`, `cargo-build`), `None` when the shell itself is in the foreground.
    pub cmd: Option<String>,
    pub cwd: PathBuf,
}

impl Probe {
    pub fn name(&self) -> String {
        format_name(&self.ctx, self.cmd.as_deref())
    }
}

/// `repo:cmd`, or just `repo` when there is no foreground command.
pub fn format_name(ctx: &str, cmd: Option<&str>) -> String {
    let n = match cmd {
        Some(c) if !c.is_empty() => format!("{ctx}:{c}"),
        _ => ctx.to_string(),
    };
    n.chars().filter(|c| !c.is_control()).take(40).collect()
}

/// Nearest ancestor of `cwd` (inclusive) containing `.git`.
pub fn find_repo(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

fn base(p: &Path) -> String {
    p.file_name().map_or_else(|| "/".to_string(), |n| n.to_string_lossy().into_owned())
}

/// Repo name if inside a git repo, else the directory name (`~` for $HOME).
pub fn context_name(cwd: &Path) -> String {
    if let Some(r) = find_repo(cwd) {
        return base(&r);
    }
    if std::env::var_os("HOME").is_some_and(|h| Path::new(&h) == cwd) {
        return "~".into();
    }
    base(cwd)
}

/// Programs whose first non-flag argument says what they are doing.
const WRAPPERS: &[&str] = &[
    "cargo", "git", "npm", "npx", "yarn", "pnpm", "make", "docker", "kubectl", "go", "ssh", "sudo", "python",
    "python3", "node", "ruby", "bundle", "systemctl", "journalctl", "mosh", "tmux", "just",
];

/// Label from a NUL-separated /proc cmdline: basename of argv[0], plus the first argument for wrappers.
pub fn cmd_label(cmdline: &[u8]) -> Option<String> {
    let mut args = cmdline.split(|&b| b == 0).filter(|a| !a.is_empty()).map(String::from_utf8_lossy);
    let argv0 = args.next()?;
    let cmd = base(Path::new(argv0.as_ref())).trim_start_matches('-').to_string();
    if cmd.is_empty() {
        return None;
    }
    if WRAPPERS.contains(&cmd.as_str())
        && let Some(a) = args.find(|a| !a.starts_with('-'))
    {
        let a = base(Path::new(a.as_ref()));
        let a: String = a.chars().filter(|c| c.is_alphanumeric() || "._@-".contains(*c)).take(20).collect();
        if !a.is_empty() {
            return Some(format!("{cmd}-{a}"));
        }
    }
    Some(cmd)
}

/// Inspect the pty's foreground process group. `shell_pid` is the session's shell.
pub fn probe(master: impl AsFd, shell_pid: u32) -> Option<Probe> {
    let pgid = nix::unistd::tcgetpgrp(master).ok()?.as_raw() as u32;
    let cwd = std::fs::read_link(format!("/proc/{pgid}/cwd"))
        .or_else(|_| std::fs::read_link(format!("/proc/{shell_pid}/cwd")))
        .ok()?;
    let cmd = if pgid == shell_pid {
        None
    } else {
        std::fs::read(format!("/proc/{pgid}/cmdline")).ok().and_then(|c| cmd_label(&c))
    };
    Some(Probe { ctx: context_name(&cwd), cmd, cwd })
}

/// Should a heuristic result `new_ctx`/`new_name` replace the current name?
/// `llm_ctx` is the context recorded when the LLM named the session.
pub fn heuristic_may_override(
    source: NameSource,
    cur_name: &str,
    llm_ctx: Option<&str>,
    new_ctx: &str,
    new_name: &str,
) -> bool {
    if cur_name == new_name {
        return false;
    }
    match source {
        NameSource::User => false,
        NameSource::Llm => llm_ctx != Some(new_ctx),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_shape_and_uniqueness() {
        let n = initial_name(&[]);
        let (a, b) = n.split_once('-').unwrap();
        assert!(ADJECTIVES.contains(&a) && NOUNS.contains(&b));
        assert_ne!(initial_name(&[&n]), n);
    }

    #[test]
    fn formatting() {
        assert_eq!(format_name("nt", Some("vim")), "nt:vim");
        assert_eq!(format_name("nt", None), "nt");
        assert_eq!(format_name("nt", Some("")), "nt");
        assert_eq!(format_name(&"x".repeat(100), None).len(), 40);
    }

    #[test]
    fn cmdlines() {
        assert_eq!(cmd_label(b"/usr/bin/vim\0main.rs\0").unwrap(), "vim");
        assert_eq!(cmd_label(b"cargo\0-q\0build\0").unwrap(), "cargo-build");
        assert_eq!(cmd_label(b"-bash\0").unwrap(), "bash");
        assert_eq!(cmd_label(b"ssh\0-p\0x\0host\0").unwrap(), "ssh-x"); // naive: first non-flag
        assert_eq!(cmd_label(b""), None);
    }

    #[test]
    fn precedence() {
        use NameSource::*;
        assert!(!heuristic_may_override(User, "a", None, "r", "r:vim"));
        assert!(heuristic_may_override(Initial, "a", None, "r", "r"));
        assert!(heuristic_may_override(Heuristic, "r", None, "r", "r:vim"));
        assert!(!heuristic_may_override(Heuristic, "r", None, "r", "r"));
        assert!(!heuristic_may_override(Llm, "fix-bug", Some("r"), "r", "r:vim"));
        assert!(heuristic_may_override(Llm, "fix-bug", Some("r"), "other", "other"));
    }

    #[test]
    fn git_walk() {
        let root = std::env::temp_dir().join(format!("nt-test-{}", std::process::id()));
        let deep = root.join("myrepo/src/a/b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(root.join("myrepo/.git")).unwrap();
        assert_eq!(find_repo(&deep).unwrap(), root.join("myrepo"));
        assert_eq!(context_name(&deep), "myrepo");
        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(find_repo(&plain).is_none_or(|r| !r.starts_with(&root)));
        assert_eq!(context_name(&plain), "plain");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
