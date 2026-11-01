//! The naming prompt shared by the local and API backends: prompt v5 (screen input, see `screen::digest`), a system
//! message plus six few-shot user/assistant turns (four first runs: a shell build, an AI agent's TUI, a shell inside a
//! container, an editor; then two rolling updates with a previous summary), followed by the real input as the last
//! user turn.

use super::namer::{sanitize_name, sanitize_recap, sanitize_summary};

pub const SYSTEM: &str = "You label and summarize terminal sessions so a user can tell them apart and search for them later.\nYou get what the session shows now: the working directory (with git repo and branch), the terminal title, the foreground program, the last shell command with its result, the text on the screen, newest at the bottom, and optionally the older history (the previous summary).\nReply with exactly three lines and nothing else:\nname: <2-4 lowercase words joined by hyphens>\ntopic: <the goal in at most 50 characters>\nsummary: <2-4 short factual sentences, at most 300 characters in total>\nRules:\n- name: the project, host or task, taken from the directory, git repo, branch, host or title. Do not just repeat the last command.\n- topic: what the session is about RIGHT NOW: take it from the bottom of the screen, the title and the cwd line, not from the older history. If the branch or directory changed, describe the new one. Not the status, not a restatement of the last command.\n- summary: what has been done in the session overall: project or host, goal, what was tried and its outcomes. Write it as past-tense history and never say 'the user'. Never state live state or durations (no 'is currently running', 'for 5 minutes', 'in progress', percentages): the status line shows what is running now, and the screen may be out of date when this is read.\n- If the input ends with 'older history', that is what happened before the screen; the screen is newer and wins where they differ. Update it: say in one short sentence what was done before, then what the screen shows now. Drop details that no longer matter. Never copy the old summary unchanged.\n- The bottom of the screen is the newest: if it shows an earlier error fixed or a command succeeding, say so; never leave a fixed problem as the current state.\n- A shell prompt with another user or host (root@4f2a9c1d7e3b, deploy@web02) means the session is inside a container or on another machine: say where.\n- Use only facts, names, numbers and paths that appear in the input. Do not invent details.\n- A program's terminal title often states the exact task (AI coding agents, editors): prefer it for topic and summary. Menus, borders, key hints and status bars of full-screen programs are not part of the work.\n- Sessions in the same repo must be told apart by what they do (frontend, database, tests, deploy...).\n- `~` is the user's home directory; `user` and `host` are placeholders for the account and machine names. Never interpret them (no topics about a user or host called that).";

/// Few-shot examples (user input, assistant answer). The last two are rolling updates.
pub const SHOTS: [(&str, &str); 6] = [
    (
        "cwd: ~/code/chatd (git chatd, branch async-rewrite)\nforeground: bash\nlast command: cargo check · exit 101\nscreen (newest at the bottom):\n| user@host:~/code/chatd$ cargo check\n|     Checking chatd v0.4.0 (~/code/chatd)\n| error[E0502]: cannot borrow `self.clients` as mutable because it is also borrowed as immutable\n|   --> src/hub.rs:88:13\n| 87 |         for c in self.clients.iter() {\n|    |                  ------------------- immutable borrow occurs here\n| 88 |             self.clients.remove(&c.id);\n|    |             ^^^^^^^^^^^^^^^^^^^^^^^^^^ mutable borrow occurs here\n| error: could not compile `chatd` (bin \"chatd\") due to 1 previous error\n| user@host:~/code/chatd$",
        "name: chatd-hub-borrow-fix\ntopic: chatd async rewrite: borrow error in hub.rs\nsummary: Worked on the async rewrite of chatd in ~/code/chatd (branch async-rewrite). cargo check failed with a borrow error in src/hub.rs: self.clients is modified while it is iterated.",
    ),
    (
        "cwd: ~/src/ledger (git ledger, branch main)\ntitle: \"Fix flaky retry test in sync.rs\"\nforeground: claude\nrunning command: claude\nscreen (newest at the bottom):\n| ● Read(src/sync.rs)\n|   ⎿  Read 412 lines\n| ● The retry test waits on a fixed 50 ms sleep, so it races the backoff timer. I'll switch it to a paused tokio clock.\n| ● Update(src/sync.rs)\n|   ⎿  Updated src/sync.rs with 6 additions and 3 removals\n| ● Bash(cargo test retry)\n|   ⎿  test sync::tests::retry_backs_off ... ok\n|      test result: ok. 1 passed; 0 failed\n|\n| >\n|   ? for shortcuts",
        "name: ledger-retry-test-fix\ntopic: fixing flaky retry test in ledger sync.rs\nsummary: Worked with Claude Code in ~/src/ledger (branch main) on the flaky retry test in src/sync.rs. The test raced the backoff timer on a fixed sleep; it was switched to a paused tokio clock and cargo test retry passes.",
    ),
    (
        "cwd: ~/work/shop (git shop, branch main)\nforeground: docker exec -it shop-db-1 bash\nrunning command: docker exec -it shop-db-1 bash\nscreen (newest at the bottom):\n| user@host:~/work/shop$ docker exec -it shop-db-1 bash\n| root@4f2a9c1d7e3b:/# psql -U shop -c \"select count(*) from orders where status = 'pending';\"\n|  count\n|   1832\n| (1 row)\n|\n| root@4f2a9c1d7e3b:/# psql -U shop -c \"vacuum analyze orders;\"\n| VACUUM\n| root@4f2a9c1d7e3b:/#",
        "name: shop-db-orders\ntopic: shop database: pending orders and vacuum\nsummary: Opened a shell in the shop-db-1 container from ~/work/shop. Counted 1832 pending orders with psql, then ran vacuum analyze on the orders table.",
    ),
    (
        "cwd: /etc/nginx/sites-available\ntitle: \"blog.conf (/etc/nginx/sites-available) - VIM\"\nforeground: vim blog.conf\nrunning command: sudo vim blog.conf\nscreen (newest at the bottom):\n| server {\n|     listen 443 ssl;\n|     server_name blog.example.org;\n|     ssl_certificate /etc/letsencrypt/live/blog.example.org/fullchain.pem;\n|     location / {\n|         proxy_pass http://127.0.0.1:2368;\n|     }\n| }\n|\n| \"blog.conf\" 9L, 214B",
        "name: nginx-blog-vhost\ntopic: editing the nginx vhost for blog.example.org\nsummary: Edited the nginx site config /etc/nginx/sites-available/blog.conf in vim with sudo. It serves blog.example.org on port 443 with a Let's Encrypt certificate and proxies to 127.0.0.1:2368.",
    ),
    (
        "cwd: ~/work/billing (git billing, branch fix-rounding)\nforeground: bash\nlast command: git push origin fix-rounding · exit 1\nscreen (newest at the bottom):\n| user@host:~/work/billing$ pytest -q\n| .........................................                                [100%]\n| 41 passed in 18.20s\n| user@host:~/work/billing$ git commit -qam \"round totals half-even\"\n| user@host:~/work/billing$ git push origin fix-rounding\n|  ! [rejected]        fix-rounding -> fix-rounding (fetch first)\n| error: failed to push some refs to 'github.com:acme/billing.git'\n| user@host:~/work/billing$\nolder history (before this screen; may be out of date): Worked on the invoice service in ~/work/billing (branch fix-rounding). pytest failed 3 tests in tests/test_totals.py with an AssertionError about rounding.",
        "name: billing-rounding-fix\ntopic: fixing invoice total rounding in billing\nsummary: Fixed rounding in the billing invoice totals (~/work/billing, branch fix-rounding). The 3 failing tests were fixed and all 41 tests pass. The fix was committed, but git push was rejected because the remote has newer commits.",
    ),
    (
        "cwd: ~\nforeground: ssh web02\nrunning command: ssh web02\nscreen (newest at the bottom):\n| deploy@web02:~$ sudo nginx -t\n| nginx: the configuration file /etc/nginx/nginx.conf syntax is ok\n| nginx: configuration file /etc/nginx/nginx.conf test is successful\n| deploy@web02:~$ sudo systemctl reload nginx\n| deploy@web02:~$ curl -sI https://blog.example.org\n| curl: (7) Failed to connect to blog.example.org port 443 after 3 ms: Couldn't connect to server\n| deploy@web02:~$\nolder history (before this screen; may be out of date): Set up the blog on web02: installed nginx and wrote a vhost for blog.example.org. nginx -t reported a syntax error.",
        "name: web02-blog-nginx\ntopic: blog on web02: nginx up, https unreachable\nsummary: Set up the blog on web02 over ssh. The nginx syntax error was fixed: nginx -t passes and nginx was reloaded. curl to https://blog.example.org still fails to connect on port 443.",
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
        assert!(m[9].1.contains("\nolder history (before this screen; may be out of date): "));
        // every few-shot answer obeys the limits the sanitizers enforce
        for (_, a) in SHOTS {
            let r = parse_answer(a);
            assert!(r.name.is_some() && r.topic.is_some() && r.summary.is_some(), "{a}");
            assert_eq!(r.summary.as_deref(), a.split("summary: ").nth(1), "summary must survive sanitizing unchanged");
            assert_eq!(super::super::namer::drop_live_state(r.summary.as_deref().unwrap(), false), r.summary, "examples must not teach live state");
            assert_eq!(r.topic.as_deref(), a.lines().nth(1).and_then(|l| l.strip_prefix("topic: ")));
        }
    }

    #[test]
    fn placeholders_are_explained() {
        let m = messages("cwd: ~\n");
        assert!(m[0].1.contains("`~` is the user's home directory") && m[0].1.contains("`user` and `host` are placeholders"));
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
