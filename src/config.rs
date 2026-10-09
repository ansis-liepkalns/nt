//! Defaults, the minimal `~/.config/nt/config.toml` reader, and runtime paths.

use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// Where names and topics come from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LlmMode {
    /// A small GGUF model run by the `nt namer` child (needs the `llm` cargo feature).
    Local,
    /// An OpenAI-compatible chat completions endpoint (`curl`; no cargo feature needed).
    Api,
    Off,
}

#[derive(Clone)]
pub struct Config {
    /// Prefix key as a byte (C-] = 0x1d).
    pub prefix: u8,
    /// Lines of scrollback kept by the server's vt100 parser.
    pub scrollback: usize,
    /// TCP port of the remote listener (`nt server --port` overrides it).
    pub port: u16,
    /// Names and topics: `local`, `api` or `off`.
    pub llm: LlmMode,
    /// GGUF model: an https URL (downloaded to the models dir on first use) or a local path.
    pub llm_model: String,
    /// Chat completions URL for `llm = "api"`.
    pub llm_api_url: String,
    pub llm_api_model: String,
    /// Environment variable (of the daemon) holding the API key.
    pub llm_api_key_env: String,
    /// File holding the API key; used when the environment variable is unset.
    pub llm_api_key_file: String,
    /// Run the local model while on battery power (default false: skip runs until AC is back).
    pub llm_on_battery: bool,
    /// Threads for the local model: `None` = auto (prompt nproc/2 up to 16, generation nproc/4 up to 8).
    pub llm_threads: Option<usize>,
    /// Inject prompt/command hooks into bash, zsh and fish sessions (live status, command log).
    pub shell_integration: bool,
    /// Show the one-time key tip when a client attaches.
    pub tips: bool,
}

/// Granite 4.0 1B Instruct, Q4_K_M (~1 GB, ~1.5 GB RSS): the best name/topic quality in bench/RESULTS.md.
/// Lighter alternative (about half the RAM): Qwen3.5-0.8B Q4_K_M,
/// `https://huggingface.co/unsloth/Qwen3.5-0.8B-GGUF/resolve/main/Qwen3.5-0.8B-Q4_K_M.gguf`.
pub const DEFAULT_LLM_MODEL: &str = "https://huggingface.co/ibm-granite/granite-4.0-1b-GGUF/resolve/main/granite-4.0-1b-Q4_K_M.gguf";
pub const DEFAULT_API_URL: &str = "https://api.openai.com/v1/chat/completions";
pub const DEFAULT_API_MODEL: &str = "gpt-5-nano";

impl Default for Config {
    fn default() -> Self {
        Config {
            prefix: 0x1d,
            scrollback: 1000,
            port: 4022,
            llm: if cfg!(feature = "llm") { LlmMode::Local } else { LlmMode::Off },
            llm_model: DEFAULT_LLM_MODEL.into(),
            llm_api_url: DEFAULT_API_URL.into(),
            llm_api_model: DEFAULT_API_MODEL.into(),
            llm_api_key_env: "OPENAI_API_KEY".into(),
            llm_api_key_file: String::new(),
            llm_on_battery: false,
            llm_threads: None,
            shell_integration: true,
            tips: true,
        }
    }
}

/// `C-]`, `C-a`, `^]`, or a single byte -> the control byte.
fn parse_prefix(s: &str) -> Option<u8> {
    let key = s.strip_prefix("C-").or_else(|| s.strip_prefix("c-")).or_else(|| s.strip_prefix('^'))?;
    let c = key.chars().next().filter(|_| key.chars().count() == 1)?.to_ascii_uppercase();
    matches!(c, '@'..='_').then(|| c as u8 - b'@')
}

/// One `key = value` line: `value` is a "quoted string" (with `\\`, `\"`, `\n`, `\t` escapes), a bare
/// number/bool/word, and may be followed by a `# comment`. Blank lines, comments and `[section]` headers are skipped.
fn parse_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
        return None;
    }
    let (k, v) = line.split_once('=')?;
    let v = v.trim();
    let val = if let Some(rest) = v.strip_prefix('"') {
        let (mut out, mut it) = (String::new(), rest.chars());
        while let Some(c) = it.next() {
            match c {
                '"' => break,
                '\\' => match it.next()? {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    c => out.push(c),
                },
                c => out.push(c),
            }
        }
        out
    } else {
        v.split('#').next().unwrap_or("").trim().to_string()
    };
    Some((k.trim().to_string(), val))
}

impl Config {
    /// Defaults overlaid with `$XDG_CONFIG_HOME/nt/config.toml` (`~/.config/nt/config.toml`), if it exists.
    /// A bad line is reported on stderr and skipped.
    pub fn load() -> Config {
        let base = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ => match std::env::var_os("HOME") {
                Some(h) => PathBuf::from(h).join(".config"),
                None => return Config::default(),
            },
        };
        let mut cfg = Config::default();
        if let Ok(text) = std::fs::read_to_string(base.join("nt/config.toml")) {
            cfg.apply(&text);
        }
        cfg
    }

    pub fn apply(&mut self, text: &str) {
        for line in text.lines() {
            let Some((k, v)) = parse_line(line) else { continue };
            let ok = match k.as_str() {
                "prefix" => parse_prefix(&v).map(|p| self.prefix = p).is_some(),
                "scrollback" => v.parse().map(|n| self.scrollback = n).is_ok(),
                "port" => v.parse().map(|n| self.port = n).is_ok(),
                "shell_integration" => v.parse().map(|b| self.shell_integration = b).is_ok(),
                "llm" => match v.as_str() {
                    "local" | "true" => Some(if cfg!(feature = "llm") { LlmMode::Local } else { LlmMode::Off }),
                    "api" => Some(LlmMode::Api),
                    "off" | "false" => Some(LlmMode::Off),
                    _ => None,
                }
                .map(|m| self.llm = m)
                .is_some(),
                "llm_threads" => match v.as_str() {
                    "auto" => { self.llm_threads = None; true }
                    _ => v.parse().ok().filter(|&n: &usize| n >= 1).map(|n| self.llm_threads = Some(n)).is_some(),
                },
                "tips" => v.parse().map(|b| self.tips = b).is_ok(),
                "llm_on_battery" => v.parse().map(|b| self.llm_on_battery = b).is_ok(),
                "llm_model" => !v.is_empty() && { self.llm_model = v; true },
                "llm_api_url" => !v.is_empty() && { self.llm_api_url = v; true },
                "llm_api_model" => !v.is_empty() && { self.llm_api_model = v; true },
                "llm_api_key_env" => { self.llm_api_key_env = v; true }
                "llm_api_key_file" => { self.llm_api_key_file = v; true }
                _ => false,
            };
            if !ok {
                eprintln!("nt: config: ignoring `{}`", line.trim());
            }
        }
    }
}

/// `$XDG_RUNTIME_DIR/nt`, or `/tmp/nt-$UID`; created with mode 0700.
pub fn runtime_dir() -> Result<PathBuf> {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("nt"),
        _ => PathBuf::from(format!("/tmp/nt-{}", nix::unistd::getuid())),
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("ctl.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_config() {
        let mut c = Config::default();
        c.apply("# c\nprefix = \"C-a\"  # x\nport = 3000\nscrollback=50\nshell_integration = false\n[sec]\nllm = \"api\"\nllm_api_model = \"m \\\"q\\\" # not comment\"\nbogus = 1\nllm_api_key_file = ~/k\n");
        assert_eq!((c.prefix, c.port, c.scrollback, c.shell_integration, c.llm), (1, 3000, 50, false, LlmMode::Api));
        assert_eq!(c.llm_api_model, "m \"q\" # not comment");
        assert_eq!(c.llm_api_key_file, "~/k");
        assert_eq!(parse_prefix("C-]"), Some(0x1d));
        assert_eq!(parse_prefix("C-"), None);
        c.apply("llm = off\nllm_model = \"/m.gguf\"");
        assert_eq!((c.llm, c.llm_model.as_str()), (LlmMode::Off, "/m.gguf"));
        assert!(!c.llm_on_battery);
        c.apply("llm_on_battery = true");
        assert!(c.llm_on_battery);
        assert_eq!((c.llm_threads, c.tips), (None, true));
        c.apply("llm_threads = 6\ntips = false");
        assert_eq!((c.llm_threads, c.tips), (Some(6), false));
        c.apply("llm_threads = \"auto\"");
        assert_eq!(c.llm_threads, None);
        c.apply("llm_threads = 0");
        assert_eq!(c.llm_threads, None);
    }

    #[test]
    fn example_config_parses() {
        let mut c = Config::default();
        c.apply(include_str!("../contrib/config.example.toml"));
        assert_eq!((c.prefix, c.port, c.llm_api_model.as_str(), c.llm_model.as_str()), (0x1d, 4022, "gpt-5-nano", DEFAULT_LLM_MODEL));
    }
}
