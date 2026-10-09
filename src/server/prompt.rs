//! The naming prompt shared by the local and API backends: prompt v3 of the benchmark (`bench/prompts/v3.json`,
//! see bench/RESULTS.md), a system message plus four few-shot user/assistant turns (three first-run digests on
//! unrelated topics and two rolling updates with a previous summary), followed by the real digest as the last user turn.

use super::namer::{sanitize_name, sanitize_recap, sanitize_summary};

pub const SYSTEM: &str = "You label and summarize terminal sessions so a user can tell them apart and search for them later.\nYou get a digest: optionally the earlier summary, the working directory, commands (oldest first, with status and duration, some with output lines), hosts, files and the foreground program.\nReply with exactly three lines and nothing else:\nname: <2-4 lowercase words joined by hyphens>\ntopic: <the goal in at most 50 characters>\nsummary: <2-4 short factual sentences, at most 300 characters in total>\nRules:\n- name: the project, host or task, taken from the directory, git repo, branch or host. Do not just repeat the last command.\n- topic: what the user is trying to achieve, not the status and not a restatement of the last command.\n- summary: what has been done in the session overall: project or host, goal, what was tried, what happened, where it stands now. Write it in plain past/present tense and never say 'the user'.\n- If the digest starts with 'earlier summary', update it: say in one short sentence what was done before, then describe the new commands and where things stand now. Drop details that no longer matter. Never copy the old summary unchanged.\n- The newest commands decide where things stand now: if an earlier error was fixed or a command now succeeds, say so; never leave a fixed problem as the current state.\n- Use only facts, names, numbers and paths that appear in the digest or the earlier summary. Do not invent details.\n- A program's terminal title (title: \"...\" on a command) often states the exact task (e.g. AI coding agents, editors) - prefer it for topic and summary. Output marked as omitted is screen-redraw noise.\n- Sessions in the same repo must be told apart by what they do (frontend, database, tests, deploy...).";

/// Few-shot examples (user digest, assistant answer). The last one is a rolling update.
pub const SHOTS: [(&str, &str); 6] = [
    (
        "cwd: ~/src/pixelforge (git pixelforge, branch gpu-resize)\ncommands (oldest first):\n  [~/src/pixelforge] cmake --build build -j8        ok      2m10s\n  [~/src/pixelforge] ./build/bench_resize           ok      30s\n      | lanczos 4096x4096: 11.2 ms\n  [~/src/pixelforge] vim shaders/resize.comp        running 9m\nfiles: shaders/resize.comp\nforeground: vim shaders/resize.comp",
        "name: pixelforge-shaders\ntopic: pixelforge: tuning GPU resize shader\nsummary: Working on the GPU resize path of pixelforge in ~/src/pixelforge (branch gpu-resize). Built the project and ran bench_resize, which reported 11.2 ms for a 4096x4096 lanczos resize. Now editing shaders/resize.comp in vim.",
    ),
    (
        "cwd: ~/infra/k8s (git infra, branch prod-rollout)\ncommands (oldest first):\n  [~/infra/k8s] kubectl get pods -n payments        ok      1s\n  [~/infra/k8s] kubectl rollout status deploy/api   exit 1  5m\n      | error: deployment \"api\" exceeded its progress deadline\n  [~/infra/k8s] kubectl logs deploy/api --tail=50   ok      1s\n      | panic: dial tcp 10.0.3.7:5432: connect: connection refused\nforeground: bash",
        "name: payments-rollout\ntopic: payments api rollout stuck, db unreachable\nsummary: Investigating a stuck rollout of the payments api from ~/infra/k8s (branch prod-rollout). kubectl rollout status failed with a progress deadline error after 5 minutes. The api logs show a panic: connection refused to 10.0.3.7:5432, so the database cannot be reached.",
    ),
    (
        "cwd: ~/Videos/trip\ncommands (oldest first):\n  [~/Videos/trip] ls                                ok      0s\n  [~/Videos/trip] ffmpeg -i clip01.mov -c:v libx265 out01.mp4 running 6m\n      | frame= 4210 fps= 31 q=28.0\nforeground: ffmpeg -i clip01.mov -c:v libx265 out01.mp4",
        "name: trip-video-encode\ntopic: encoding trip videos to h265\nsummary: Converting trip videos in ~/Videos/trip. Listed the folder, then started encoding clip01.mov to out01.mp4 with ffmpeg and libx265. The encode has been running for 6 minutes at about 31 fps.",
    ),
    (
        "cwd: ~/src/ledger (git ledger, branch main)\ncommands (oldest first):\n  [~/src/ledger] git status          ok      0s\n  [~/src/ledger] codeagent           running 12m  title: \"Fix flaky retry test in sync.rs\" (earlier: \"Explore repo layout\", \"Run test suite\")  (interactive TUI, output omitted)\nforeground: codeagent",
        "name: ledger-agent-retry-fix\ntopic: fixing flaky retry test in ledger sync.rs\nsummary: Working in ~/src/ledger (branch main) with an AI coding agent that has been running for 12 minutes. It explored the repo, ran the test suite and is now fixing the flaky retry test in sync.rs.",
    ),
    (
        "earlier summary: Setting up the blog on web02. Installed nginx with apt and wrote a vhost for blog.example.org; the first nginx -t reported a syntax error.\ncwd: ~\nnew commands since then (oldest first):\n  [~] sudo vim /etc/nginx/sites-available/blog   ok      5m\n  [~] sudo nginx -t                              ok      0s\n      | nginx: configuration file /etc/nginx/nginx.conf test is successful\n  [~] sudo systemctl reload nginx                ok      0s\n  [~] curl -sI https://blog.example.org          exit 7  0s\n      | curl: (7) Failed to connect to blog.example.org port 443\nhosts: web02\nforeground: bash",
        "name: web02-blog-nginx\ntopic: setting up the blog site on web02 with nginx\nsummary: Setting up the blog on web02. Installed nginx and wrote a vhost for blog.example.org. After editing the vhost, nginx -t passes and nginx was reloaded, but curl to https://blog.example.org still fails to connect on port 443.",
    ),
    (
        "earlier summary: Working on the invoice service in ~/work/billing (branch fix-rounding). Ran pytest, which failed 3 tests in tests/test_totals.py with an AssertionError about rounding. Opened totals.py in vim.\ncwd: ~/work/billing (git billing, branch fix-rounding)\nnew commands since then (oldest first):\n  [~/work/billing] vim billing/totals.py                    ok      12m\n  [~/work/billing] pytest -q                                ok      20s\n      | 41 passed in 18.20s\n  [~/work/billing] git commit -am \"round totals half-even\"   ok      0s\n  [~/work/billing] git push origin fix-rounding             exit 1  2s\n      | ! [rejected] fix-rounding -> fix-rounding (fetch first)\nfiles: billing/totals.py\nforeground: bash",
        "name: billing-rounding-fix\ntopic: fixing invoice total rounding in billing\nsummary: Fixing rounding in the billing invoice totals (~/work/billing, branch fix-rounding). Three tests in tests/test_totals.py failed on rounding; after editing billing/totals.py all 41 tests pass. Committed the fix, but git push was rejected because the remote has newer commits.",
    ),
];

/// Longest answer asked for: name + topic (~30 tokens) and a 300-character summary (~75 tokens).
pub const MAX_NEW_TOKENS: usize = 130;

/// `(role, content)` chat messages for one digest.
pub fn messages(digest: &str) -> Vec<(&'static str, String)> {
    let mut m = vec![("system", SYSTEM.to_string())];
    for (u, a) in SHOTS {
        m.push(("user", u.to_string()));
        m.push(("assistant", a.to_string()));
    }
    m.push(("user", digest.trim_end().to_string()));
    m
}

/// What a reply yields; each field is `None` when it is missing or unusable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reply {
    pub name: Option<String>,
    /// The topic (stored as the session's `recap`).
    pub topic: Option<String>,
    pub summary: Option<String>,
}

/// The first `name:` line, the first `topic:` line after it and the first `summary:` line after that
/// (plus wrapped continuation lines up to a blank line); the rest is ignored.
pub fn parse_answer(reply: &str) -> Reply {
    let (mut r, mut seen_name, mut summary): (Reply, bool, Option<String>) = (Reply::default(), false, None);
    for line in reply.lines() {
        let line = line.trim();
        if let Some(sum) = summary.as_mut() {
            if line.is_empty() || line.starts_with("name:") || line.starts_with("topic:") {
                break;
            }
            sum.push(' ');
            sum.push_str(line);
        } else if !seen_name {
            if let Some(v) = line.strip_prefix("name:") {
                seen_name = true;
                r.name = sanitize_name(v);
            }
        } else if let Some(v) = line.strip_prefix("topic:") {
            if r.topic.is_none() {
                r.topic = sanitize_recap(v);
            }
        } else if let Some(v) = line.strip_prefix("summary:") {
            summary = Some(v.trim().to_string());
        }
    }
    r.summary = summary.and_then(|s| sanitize_summary(&s));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(n: Option<&str>, t: Option<&str>, s: Option<&str>) -> Reply {
        Reply { name: n.map(Into::into), topic: t.map(Into::into), summary: s.map(Into::into) }
    }

    #[test]
    fn messages_shape() {
        let m = messages("cwd: ~/nt\n");
        assert_eq!(m.len(), 14);
        assert_eq!((m[0].0, m[13].0, m[13].1.as_str()), ("system", "user", "cwd: ~/nt"));
        // a rolling example carries a previous summary
        assert!(m[9].1.starts_with("earlier summary: "));
        // every few-shot answer obeys the limits the sanitizers enforce
        for (_, a) in SHOTS {
            let r = parse_answer(a);
            assert!(r.name.is_some() && r.topic.is_some() && r.summary.is_some(), "{a}");
            assert_eq!(r.summary.as_deref(), a.split("summary: ").nth(1), "summary must survive sanitizing unchanged");
            assert_eq!(r.topic.as_deref(), a.lines().nth(1).and_then(|l| l.strip_prefix("topic: ")));
        }
    }

    #[test]
    fn answers() {
        let r = parse_answer("name: Fix Build\ntopic: nt reconnect: proto type fixes\nsummary: Fixed a type error in proto.rs. Tests pass now.\nname: other-name\ntopic: ignored line here");
        assert_eq!(r, reply(Some("fix-build"), Some("nt reconnect: proto type fixes"), Some("Fixed a type error in proto.rs. Tests pass now.")));
        assert_eq!(parse_answer("nonsense"), Reply::default());
        // topic before name is ignored; a junk name keeps the topic and the summary
        let r = parse_answer("topic: building the crate now\nname: ok-name\ntopic: real topic here\nsummary: Built the crate in ~/src/nt and ran the tests.");
        assert_eq!(r, reply(Some("ok-name"), Some("real topic here"), Some("Built the crate in ~/src/nt and ran the tests.")));
        assert_eq!(parse_answer("name: a-b-c-d-e\ntopic: building the crate now").name, None);
        // old two-line replies still work, wrapped summaries are joined, a blank line ends the reply
        assert_eq!(parse_answer("name: a-b\ntopic: building the crate now"), reply(Some("a-b"), Some("building the crate now"), None));
        let r = parse_answer("name: a-b\ntopic: building the crate\nsummary: Built the crate in ~/src/nt.\n  Tests pass.\n\nsummary: ignored");
        assert_eq!(r.summary.as_deref(), Some("Built the crate in ~/src/nt. Tests pass."));
        // an unusable summary is dropped, the rest is kept
        assert_eq!(parse_answer("name: a-b\ntopic: building the crate\nsummary: ok"), reply(Some("a-b"), Some("building the crate"), None));
    }
}
