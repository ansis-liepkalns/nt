//! OpenAI-compatible chat completions backend for names, topics and summaries (`llm = "api"`). It runs inside the
//! `nt namer` child, shells out to `curl` (no HTTP dependency) and needs no inference code, so it works
//! without the `llm` cargo feature.
//!
//! Privacy: the digest is passed through `redact` first (secret-looking values never leave the machine), and the
//! API key reaches curl on stdin (`-K -`), never on the command line where `ps` could show it.
//!
//! JSON is built and read by hand: the request is a fixed shape of strings, and from the reply we need exactly
//! one string (`choices[0].message.content`). That is a few dozen lines, so serde_json is not pulled in.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use super::namer::Job;
use super::prompt;
use crate::config::Config;

/// What a backend returns for one job: the parsed reply or why it failed.
pub type Answer = Result<prompt::Reply>;

const TIMEOUT_SECS: u32 = 20;

// ---------------------------------------------------------------- redaction

const SENSITIVE: [&str; 8] = ["key", "token", "secret", "passw", "pwd", "credential", "auth", "bearer"];

fn sensitive_name(name: &str) -> bool {
    let n = name.to_lowercase();
    SENSITIVE.iter().any(|s| n.contains(s))
}

fn blob_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '_' | '=' | '-')
}

/// A long hex / base64-looking run, or a well-known token prefix.
fn looks_like_secret(w: &str) -> bool {
    let w = w.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if w.len() >= 32 && w.chars().all(blob_char) {
        let hex = w.chars().all(|c| c.is_ascii_hexdigit());
        let mixed = w.chars().any(|c| c.is_ascii_digit()) && w.chars().any(|c| c.is_ascii_alphabetic());
        return hex || mixed;
    }
    w.len() >= 16
        && ["sk-", "ghp_", "gho_", "ghs_", "github_pat_", "glpat-", "xoxb-", "xoxp-", "AKIA", "AIza", "npm_", "pypi-"]
            .iter()
            .any(|p| w.starts_with(p))
}

/// `scheme://user:pass@host` -> `scheme://***@host`.
fn redact_url_creds(w: &str) -> String {
    if let Some(i) = w.find("://") {
        let rest = &w[i + 3..];
        let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        if let Some(at) = rest[..host_end].rfind('@')
            && rest[..at].contains(':')
        {
            return format!("{}***{}", &w[..i + 3], &rest[at..]);
        }
    }
    w.to_string()
}

fn redact_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    // Set while inside a quoted value that continues over several words, and after `--password`-style flags
    // or `Bearer`.
    let mut quote: Option<char> = None;
    let mut next_secret = false;
    let mut rest = line;
    while !rest.is_empty() {
        let ws_len = rest.len() - rest.trim_start().len();
        out.push_str(&rest[..ws_len]);
        rest = &rest[ws_len..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let w = &rest[..end];
        rest = &rest[end..];
        if w.is_empty() {
            continue;
        }
        if let Some(q) = quote {
            if w.ends_with(q) {
                quote = None;
            }
            out.push_str("***");
            continue;
        }
        if next_secret {
            // `Authorization: Bearer <token>`: the scheme word is not the secret.
            if w.eq_ignore_ascii_case("bearer") || w.eq_ignore_ascii_case("basic") {
                out.push_str(w);
            } else {
                next_secret = false;
                out.push_str("***");
            }
            continue;
        }
        let w2 = redact_url_creds(w);
        if w.eq_ignore_ascii_case("bearer") || w.eq_ignore_ascii_case("bearer:") {
            next_secret = true;
            out.push_str(w);
        } else if let Some((k, v)) = w2.split_once('=').or_else(|| w2.strip_suffix(':').map(|k| (k, ""))).filter(|(k, _)| sensitive_name(k)) {
            let sep = if w2.contains('=') { '=' } else { ':' };
            if v.is_empty() && sep == ':' {
                next_secret = true; // `API_KEY: value`
                out.push_str(w);
                continue;
            }
            if let Some(q) = v.chars().next().filter(|c| matches!(c, '"' | '\''))
                && (v.len() < 2 || !v.ends_with(q))
            {
                quote = Some(q);
            }
            out.push_str(&format!("{k}{sep}***"));
        } else if w2.starts_with("--") && sensitive_name(&w2) && !w2.contains('=') {
            next_secret = true; // `--password hunter2`
            out.push_str(&w2);
        } else if looks_like_secret(&w2) {
            out.push_str("***");
        } else {
            out.push_str(&w2);
        }
    }
    out
}

/// Masks secret-looking values in a digest: `key=value` with key/token/secret/password-like names, `Bearer`
/// tokens, `--password x` flags, `user:pass@` in URLs, long hex/base64 blobs and well-known token prefixes.
pub fn redact(text: &str) -> String {
    let mut o: Vec<String> = text.lines().map(redact_line).collect();
    if text.ends_with('\n') {
        o.push(String::new());
    }
    o.join("\n")
}

// ---------------------------------------------------------------- JSON

fn json_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Reasoning models (gpt-5*, o1/o3/o4*) reject `temperature` and `max_tokens`.
fn is_reasoning_model(model: &str) -> bool {
    let m = model.rsplit('/').next().unwrap_or(model);
    m.starts_with("gpt-5") || (m.starts_with('o') && m[1..].starts_with(|c: char| c.is_ascii_digit()))
}

pub fn request_body(model: &str, digest: &str) -> String {
    let mut b = String::from("{\"model\":");
    json_str(model, &mut b);
    b.push_str(",\"messages\":[");
    for (i, (role, content)) in prompt::messages(digest).iter().enumerate() {
        if i > 0 {
            b.push(',');
        }
        b.push_str(&format!("{{\"role\":\"{role}\",\"content\":"));
        json_str(content, &mut b);
        b.push('}');
    }
    b.push(']');
    if is_reasoning_model(model) {
        // The cap covers hidden reasoning tokens too, so it is generous; "minimal" keeps them near zero.
        b.push_str(",\"reasoning_effort\":\"minimal\",\"max_completion_tokens\":400");
    } else {
        b.push_str(&format!(",\"temperature\":0,\"max_tokens\":{}", prompt::MAX_NEW_TOKENS));
    }
    b.push('}');
    b
}

fn hex4(s: &[u8]) -> Option<u32> {
    u32::from_str_radix(std::str::from_utf8(s.get(..4)?).ok()?, 16).ok()
}

/// The JSON string literal starting at `s[0] == '"'` -> its decoded value.
fn parse_json_string(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.first() != Some(&b'"') {
        return None;
    }
    let (mut out, mut i) = (Vec::new(), 1);
    while i < b.len() {
        match b[i] {
            b'"' => return String::from_utf8(out).ok(),
            b'\\' => {
                i += 1;
                match *b.get(i)? {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'u' => {
                        let mut cp = hex4(b.get(i + 1..)?)?;
                        i += 4;
                        if (0xd800..0xdc00).contains(&cp)
                            && b.get(i + 1..i + 3) == Some(b"\\u")
                            && let Some(lo) = hex4(b.get(i + 3..)?).filter(|l| (0xdc00..0xe000).contains(l))
                        {
                            cp = 0x10000 + ((cp - 0xd800) << 10) + (lo - 0xdc00);
                            i += 6;
                        }
                        let mut buf = [0; 4];
                        out.extend_from_slice(char::from_u32(cp).unwrap_or('\u{fffd}').encode_utf8(&mut buf).as_bytes());
                    }
                    c => out.push(c),
                }
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    None
}

/// `choices[0].message.content`: the first `"content"` key after the first `"message"` key (strings in the
/// reply are escaped, so a literal `"content"` can only be a key).
pub fn parse_reply(body: &str) -> Result<String> {
    let found = body.find("\"message\"").and_then(|m| {
        let after = &body[m..];
        let c = after.find("\"content\"")? + "\"content\"".len();
        let v = after[c..].trim_start().strip_prefix(':')?.trim_start();
        parse_json_string(v)
    });
    match found {
        Some(s) => Ok(s),
        None => {
            let msg: String = body.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(200).collect();
            bail!("no completion in reply: {msg}")
        }
    }
}

// ---------------------------------------------------------------- curl

fn curl_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\r', "\\r").replace('\t', "\\t")
}

fn api_key(cfg: &Config) -> Result<String> {
    let env = std::env::var(&cfg.llm_api_key_env).unwrap_or_default();
    let key = if !env.trim().is_empty() {
        env
    } else if !cfg.llm_api_key_file.is_empty() {
        let p = match cfg.llm_api_key_file.strip_prefix("~/") {
            Some(r) => std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(r),
            None => cfg.llm_api_key_file.clone().into(),
        };
        std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?
    } else {
        bail!("no API key: ${} is unset and llm_api_key_file is empty", cfg.llm_api_key_env)
    };
    let key = key.trim().to_string();
    if key.is_empty() || key.chars().any(|c| c.is_control()) {
        bail!("empty or malformed API key");
    }
    Ok(key)
}

/// One completion for `digest` (already redacted by the caller or not: this redacts again, it is idempotent).
fn complete(cfg: &Config, key: &str, digest: &str) -> Result<String> {
    if !(cfg.llm_api_url.starts_with("https://") || cfg.llm_api_url.starts_with("http://")) {
        bail!("llm_api_url must be http(s)");
    }
    let body = request_body(&cfg.llm_api_model, &redact(digest));
    let conf = format!(
        "url = \"{}\"\nrequest = \"POST\"\nheader = \"Content-Type: application/json\"\nheader = \"Authorization: Bearer {}\"\ndata-binary = \"{}\"\n",
        curl_quote(&cfg.llm_api_url),
        curl_quote(key),
        curl_quote(&body)
    );
    let mut child = Command::new("curl")
        .args(["-sS", "-K", "-", "--max-time", &TIMEOUT_SECS.to_string(), "--proto", "=http,https"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning curl")?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let w = std::thread::spawn(move || stdin.write_all(conf.as_bytes()));
    let out = child.wait_with_output()?;
    let _ = w.join();
    if !out.status.success() {
        bail!("curl: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    parse_reply(&String::from_utf8_lossy(&out.stdout))
}

/// Name, topic and summary for each job; an error on the first request (auth, network) aborts the batch so the
/// daemon backs off, later per-job failures only skip that job.
pub fn run(cfg: &Config, jobs: &[Job], mut emit: impl FnMut(&Job, Answer)) -> Result<()> {
    let key = api_key(cfg)?;
    for (i, j) in jobs.iter().enumerate() {
        match complete(cfg, &key, &j.text) {
            Ok(reply) => emit(j, Ok(prompt::parse_answer(&reply))),
            Err(e) if i == 0 => return Err(e),
            Err(e) => emit(j, Err(e)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts() {
        let d = "cwd: ~/x\n  [~/x] curl -H 'Authorization: Bearer abc.def-123' https://u:p%40ss@host.io/a?b=1  ok 1s\n  [~/x] API_KEY=sk-live-12345 ./run --password hunter2 --user bob\n  [~/x] export GITHUB_TOKEN=\"abc def\" && echo\n      | token: zzz\n      | sha 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08 built\n  [~/x] cargo build --release ok 3s\n";
        let r = redact(d);
        for leak in ["abc.def-123", "u:p%40ss", "sk-live", "hunter2", "abc def", "zzz", "9f86d081", "def\""] {
            assert!(!r.contains(leak), "{leak} leaked: {r}");
        }
        assert!(r.contains("https://***@host.io/a?b=1") && r.contains("--user bob") && r.contains("cargo build --release ok 3s"));
        assert!(r.starts_with("cwd: ~/x\n") && r.ends_with("ok 3s\n"));
        assert_eq!(redact("keyboard layout setup"), "keyboard layout setup");
    }

    #[test]
    fn json_roundtrip() {
        let b = request_body("gpt-5-nano", "a \"q\" \\ \n\t\u{1}é");
        assert!(b.contains("a \\\"q\\\" \\\\ \\n\\t\\u0001é") && b.contains("max_completion_tokens") && !b.contains("temperature"));
        assert!(request_body("gpt-4.1-nano", "x").contains("\"temperature\":0,\"max_tokens\":130"));
        let reply = r#"{"id":"x","choices":[{"index":0,"message":{"role":"assistant","content":"name: a-b\ntopic: \"q\" \u00e9 \ud83d\ude00","refusal":null},"finish_reason":"stop"}]}"#;
        assert_eq!(parse_reply(reply).unwrap(), "name: a-b\ntopic: \"q\" é 😀");
        assert!(parse_reply(r#"{"error":{"message":"bad key","type":"x"}}"#).unwrap_err().to_string().contains("bad key"));
        assert!(parse_reply(r#"{"choices":[{"message":{"content":null}}]}"#).is_err());
    }
}
