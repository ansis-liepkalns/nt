# nt: design

`nt` ("nee-tee") is a lightweight, reconnectable remote terminal written in Rust. It has built-in session listing and switching,
and session names that the server picks on its own. It does not split panes.

## Goals / non-goals
- Goals: survive network loss and roaming, reconnect on its own, list/switch/rename/kill sessions from inside nt, allow several
  clients on one session, Emacs-style keys, work in an 80x24 terminal, small binary, few dependencies.
- Non-goals: panes or windows, copy mode, scripting, Windows support.
- Platforms: Linux for the server. The client targets Linux and macOS (best effort).

## CLI (single binary `nt`, args parsed with `lexopt`)
```
nt [<ssh-dest>] [-- ssh args]     start a new session on the local daemon (unix socket, no crypto; started if needed),
                         or remote: bootstrap over ssh, then direct TCP
nt -s | --sessions [<ssh-dest>]  open the session list (switcher); with no sessions, start a new one
nt ls [<ssh-dest>]       print sessions, one line each: id, name, status, topic; grouped like the switcher (header line per group,
                         members indented by 2, ungrouped sessions last)
nt restart [<ssh-dest>]  stop a running daemon of any version and start the current one (ends its sessions);
                         remote = `nt restart` over ssh, found the same way as for bootstrap
nt -h | --help | help, nt -V | --version
nt server [--foreground] [--restart] [--port N] [--no-shell-integration]
                         (hidden) the daemon (daemonizes itself unless --foreground);
                         --restart first stops a running daemon of any version (SIGTERM, found through the socket's peer pid);
                         `nt restart` locally is the same
nt bootstrap             (hidden; run through ssh) ensure the daemon runs, register the client, print credentials
nt namer                 (hidden) short-lived naming child process, see Naming
nt name-test <file>...   (hidden) run the naming model on digest files (used for the benchmark and debugging)
```
`nt` always creates a new session. `nt -s` opens the switcher with the most recently used session preselected. Arguments are parsed
by the pure function `cli::parse`; unknown arguments give a one-line error and "see nt --help". `C-] ?` shows a small centered
key list (`client/help.rs`), drawn over the current screen or switcher and removed by repainting from the mirror.

## Server
- One daemon per user, tokio `current_thread`, holding every session.
- Control socket `$XDG_RUNTIME_DIR/nt/ctl.sock`, falling back to `/tmp/nt-$UID/ctl.sock` (mode 0700). It speaks the same framed
  protocol as TCP, in plaintext, plus `Register`.
- TCP listener on `0.0.0.0:4022` by default (config `port`). It only accepts registered client keys.
- Session = PTY (spawns `$SHELL -l`, `TERM` from the first client, `NT_SESSION=<id>` in the environment) + a `vt100::Parser`
  (screen state, 1000 lines of scrollback) + an output ring buffer (1 MiB) addressed by absolute byte offset.
- PTY: `nix::pty::openpty` + `std::process::Command` with `pre_exec` running setsid + TIOCSCTTY. The master fd is non-blocking in
  `tokio::io::unix::AsyncFd`. PTY size = the size of the attached client that most recently sent input or a resize.
- When the shell exits the session does not end: it becomes `exited`, keeping its screen, ring, transcript, name, topic and last cwd
  (sampled from /proc on every Enter and every naming tick). Input is ignored. `C2S::Respawn` starts `$SHELL -l` again in that cwd
  (same id, transcript continues, attached clients get a `Redraw`). `C2S::Kill` on an exited session ends it at once; on a live one
  it SIGHUPs the shell and the session ends when the shell is gone. The clients never ask for confirmation on an exited session. Only closing sends `SessionEnded` (attached clients return to
  the switcher, or exit if it was the last session).
- Per-session CPU and RSS (process tree, from /proc) are sampled every 2 s while any client has the switcher open (every 30 s otherwise) and
  pushed with `Sessions`. `Watch(true)` refreshes at once what the idle sampling left stale: RSS and the git branches are read immediately, the
  CPU baseline is taken and the first CPU figure follows 0.7 s later (a figure averaged over the 30 s idle window would be meaningless).
  A queued `Sessions` message is replaced by a newer one, so a slow link never delivers old numbers.
- **Branch**: the server reads the git branch of each shell's cwd from `.git/HEAD` (worktrees through the `.git` file) on every Enter, OSC 133 `D`, naming
  tick and, while a switcher is open, every sampling tick. A change marks the session dirty and `ctx_dirty` (a branch or group change is news by itself:
  no new command is needed for a new summary). `SessionInfo.branch` is shown by the clients.

## Wire protocol (`src/proto.rs`)
Frame = `u32 BE length` + payload; payload = `postcard`-encoded message (sealed over TCP). Max frame 1 MiB. Main messages:

```rust
enum C2S {
  Hello { proto, last_input_seq_acked },          // first message on every connection
  List, Detach, Ping,
  Attach { session: Option<SessionId> /*None = new*/, cols, rows, term, resume_from: Option<u64> },
  Input { seq: u64, data: Vec<u8> },              // seq increases monotonically per client identity
  Resize { cols, rows },
  Rename { session, name },                       // empty name = back to automatic naming
  Kill { session }, Respawn { session },
  Search { query },
  // plus switcher-open notification and local-only Register
}
enum S2C {
  Welcome { proto, last_input_seq },              // the client drops queued input with seq <= this
  Sessions(Vec<SessionInfo>),                     // reply to List; also pushed when names, status or the set change
  Attached { session },
  Output { offset, data },                        // offset = absolute stream position of data[0]
  Redraw { offset, data },                        // full repaint from the vt100 state; the stream continues at offset
  SessionEnded { session },
  SearchResults(Vec<..>), Error(String), Pong,
}
```
`SessionInfo` carries id, name, name source (`Initial | Heuristic | Llm | User`; `User` is locked), timestamps, client count, size,
cwd, foreground command, status, topic (`recap`), summary, exited flag, cpu and rss, `group: Option<GroupInfo {key, label, shown}>`, `branch`, `summary_unix` and `summary_stale`. `PROTO_VERSION` is 7 (`branch`, `summary_unix`, `summary_stale`; before: `group`; before: `pending`, `S2C::NamerError`, `S2C::NamerPaused`).

Version mismatch: `Hello` is the first variant of `C2S` and `proto` its first field, and the server decodes only that prefix of the first
frame (`FrameReader::read_hello`), so any peer version is recognised even if other fields changed. On a mismatch the server sends
`S2C::Error("nt version mismatch: server protocol X, client Y. A different nt version is running with N sessions; run `nt restart` …")`
and closes; the client prints it and exits instead of reconnecting (`VersionMismatch`). Older daemons just hang up; a local client
reports that as a probable version mismatch too. This covers `nt bootstrap` and local autostart, which both go through `open_link`.

Resume: on `Attach` with `resume_from = Some(o)`, if `o` is still in the ring the server replays `Output` from `o`, which keeps the
client's native scrollback intact. Otherwise it sends `Redraw`. Switching sessions always sends `Redraw`.

## Remote connection and security
1. The client runs `ssh <dest> [ssh args] nt bootstrap` (`NT_REMOTE_CMD` overrides `nt`). Bootstrap starts the daemon if needed,
   sends `Register` over ctl.sock and prints one line: `NT1 <port> <client_id hex16> <key hex32>`. The daemon keeps registrations in
   memory and drops them after 7 days unused.
2. The client resolves the real hostname with `ssh -G <dest>` and connects over TCP to `hostname:port`.
3. Handshake: client sends `client_id(16) || nonce_c(16)` in plaintext; server answers `nonce_s(16)`. Keys:
   `k_c2s = blake3::derive_key("nt v1 c2s", key||nonce_c||nonce_s)`, and `k_s2c` likewise with "nt v1 s2c". Frames are sealed with
   ChaCha20-Poly1305, nonce = 4 zero bytes || u64 BE counter per direction. The first sealed frame from the client must be
   `Hello`; if it fails to decrypt the connection drops.
4. Liveness: the client pings every 5 s; 15 s of silence means the connection is dead. The server sets TCP keepalive.
5. Reconnect: a status line `reconnecting to <dest>… (C-] q quit)` on the bottom row, retry with backoff (0.5 s doubling to 5 s),
   same client_id/key (no ssh needed). After `Welcome` the client resends unacked input, then attaches with `resume_from`. If the
   registration is gone (server restarted), it bootstraps over ssh again.
6. Input reliability: the client keeps `Input` messages queued until acknowledged; the server remembers `last_input_seq` per
   client_id and drops duplicates.

Trust model: ssh authenticates who may register a key; the key authenticates and encrypts the TCP stream. The control socket is
plaintext but sits in a 0700 directory.

## Client (`src/client/`)
- crossterm raw mode. The client feeds everything it receives into its own `vt100::Parser` (a mirror), so after an overlay closes it
  repaints locally without a round trip.
- Passthrough by default; prefix key `C-]` (configurable): `C-] C-]`/`s` switcher, `n`/`p` next/previous session, `l` last,
  `c` new, `r` rename, `k` close (y/n on a live session; an exited one closes at once), `d` detach, `q` quit, `]` literal prefix.
- **Groups** (`server/groups.rs`; the server computes them because it knows `$HOME` and the disk; `infos()` recomputes them for every push, and the
  naming tick pushes when a session changed group). Project root of a session = the *outermost* ancestor-or-self of its shell cwd (read from
  /proc, so a foreground program does not matter) holding `.git` (directory or file, for worktrees) or one of `Cargo.toml package.json
  pyproject.toml go.mod Makefile CMakeLists.txt .hg`; only strictly below `$HOME` when the cwd is under it, and never `/`, `/home`, `/tmp`,
  `/root` or `$HOME` itself. Nested repos are therefore absorbed by the outermost project. Sessions without a project: those whose cwds are
  nested form a group anchored at the shallowest such cwd, unless that is `$HOME`, `/`, `/tmp`, `/home`, a depth-1 path or an ancestor of some
  session's project root (a shell in `~/src` does not swallow `~/src/webshop`); a lone one stays ungrouped. Single-session projects still get a
  group. `GroupInfo.key` is the absolute root, `label` its basename (the whole path outside `$HOME`), `shown` the `~`-abbreviated path (the client
  does not know the server's home).
  Display: a header per group (bold label, dim path, status icons worst first `✗ ⏵ ✎ ✓` deduped and colored, summed CPU/RAM in the same
  bar/number layout as session rows; at 50 columns and below only label, icons and a heat glyph) and the dim current branch `⎇ feature/coupons` after the path
  (the first member's, `+N` when members are on other branches; `br:` in ASCII mode). Ungrouped sessions in a repository show it after their path.
  **Column alignment**: every row ends with a fixed 4-column right-aligned age; the usage cell (same layout tier as the rows) ends one column before it on group
  headers, members and ungrouped rows alike. Layout tiers by box width: >= 70 bars with numbers, 51-69 bars, <= 50 one heat glyph. Members are indented by 2, show `subdir/`
  (relative to the root) instead of the cwd and lose a redundant `<label>:` prefix of heuristic names. Ungrouped sessions follow, in the old
  format. One blank line separates blocks (groups, and each ungrouped session) when everything including status and topic lines fits, never
  between the members of a group. Order, snapshotted when the switcher opens like the session order: the current session's group, then groups by
  the most recent use of any member, then ungrouped sessions; inside a group the current session first, then recency. New sessions join the end of
  their group, new groups go after the existing groups. Under a filter, groups and members rank by best match; the group's label and path are part
  of each member's haystack, so a group name shows the whole group and other terms only the matching members (with their header). Sort modes
  order members by load and groups by their summed load. `C-s` search is not grouped.
  Navigation: `↑ ↓ C-p C-n` move over sessions across groups, skipping headers. `←` goes to the *group level* (the selected session's group; an
  ungrouped session is its own entry), where `↑ ↓` move between groups and ungrouped entries; `→` there enters the group on its most recently
  used session; `RET` on a group header attaches to that session. `→` at the session level and both arrows in search mode do nothing and
  `←/→` no longer move the filter cursor (`C-b C-f` do). A selected header has the same `▌` gutter as a selected session and the summary area
  lists the members' topics instead of a summary. Renaming and closing need a session, so they do nothing on a group header.
- Switcher overlay: whole screen below 50 rows, otherwise a centered box of at most 100x30; usable at 40x10. Each entry shows
  `#id name`, client count, foreground command, cwd, age, and a usage cell (CPU/RAM bars, narrower layouts degrade to a one-glyph
  heat, `NT_ASCII=1` or a non-UTF-8 locale uses ASCII). When there is room each entry also shows the status line and the topic.
  Typing filters (fuzzy subsequence over id/name/command/cwd/status/topic; each term may instead be a plain case-insensitive
  substring of the summary, because a subsequence over 300 characters of prose matches nearly anything) with Emacs line editing.
  `M-s` cycles sort (recent, CPU, RAM).
  **Order and look**: alt-tab semantics. The order is snapshotted when the switcher opens: the current session first (marked `*`), then the
  others by `SessionInfo.last_used_unix` (last attach or input) descending, exited sessions last; it stays fixed while open (updates change
  contents, new sessions are appended, removed ones disappear). The current session (1st entry) is preselected; without one the most recently used session (also the 1st). `C-] l` jumps back to the previously attached session of this client.
  A filter ranks by score with that order as the
  tiebreak. The selected entry is not reverse video: column 0 carries a cyan `▌` (`>` in ASCII mode) on all its lines, its `#id name` is bold, its
  status and topic are normal text (other entries' topics and plain statuses are dim), colored status symbols stay as they are.
  `C-s` toggles full-text search: the query goes to the server (`Search`, debounced 150 ms), which does a case-insensitive substring
  search over the summaries and the transcripts, newest first, at most 3 hits per session and 50 in total (a summary match comes first
  and is shown as `summary: …snippet`).
  **Summary area**: the rows between the list and the hint line are reserved for the SELECTED session's summary, word-wrapped to the
  box width, at most 3 lines (the last ends in `…` if cut), dim. Its last line ends with `· updated 4m ago`, or `· ↻ updating…` and the spinner while
  the session's naming run is pending; a summary that predates a command that started or ended after it (`summary_stale`, also set by a branch/group change)
  is prefixed `(before last command)` until it is refreshed. Rows are never added per entry. It is shown only when the box has at
  least 20 rows, the list keeps room for up to 8 entries above it and some session has a summary; otherwise it is hidden and `M-i`
  is not advertised. `M-i` toggles it (default on).
  **Closing**: `C-k` closes the selected session when the filter cursor is at the end of the filter text (where kill-line would do
  nothing; always kill-line in full-text search mode), otherwise it is kill-line. `M-k` always closes. A live session asks y/n; an exited
  one is closed at once.
- An exited session shows `shell exited · RET new shell · C-k close` on the bottom row; RET sends `Respawn`, `C-k` or `M-k` (no prefix)
  sends `Kill` at once (no y/n, the exit was the first step). `C-] k` does the same on an exited session.

## Naming, status, topic and summary
- Initial name: random `adjective-noun`, source `Initial`.
- Heuristic, every 10 s for sessions with output in the last 60 s: foreground process group (`tcgetpgrp` on the master) ->
  `/proc/<pgid>/cmdline` and `/proc/<pid>/cwd`; git repo name by walking up to `.git`. Name `repo:cmd` or `dir:cmd`; just `repo`/`dir`
  when the shell itself is in the foreground. It never overrides `Llm` unless the repo/dir changed, and never overrides `User`.
- **Status** (deterministic, live, never produced by the LLM), computed on every naming tick and on OSC 133 events and pushed to
  clients (at most 1/s per session): `⏵ make -j8 · 3m12s` running, `✎ vim src/proto.rs` in an alt-screen app, `✓ cargo build · 2m ago`,
  `✗ cargo test (101) · 5s ago` (+ first error line when there is room), `idle · ~/src/nt`. Durations are placeholders
  (`{dur}`/`{ago}`) filled by the clients from `status_since`.
- **Topic** (LLM): what the session is about *right now* in at most 50 characters (newest commands, current branch and title decide; the rolling summary is history
  and must not override them), describing the work and not the status, and not restating the last command. When the branch changed since the earlier summary the
  digest says `branch changed since the earlier summary: A -> B`.
- **Summary** (LLM): past-tense history of what was done and its outcomes. It must not state live state or durations; the status line shows that. The daemon
  enforces it: `namer::drop_live_state` removes sentences with `currently`, `is running`, `still running`, `in progress`, `is building` or `for N minutes`
  from a new summary when no command is running, and from the earlier summary before it is fed back into the digest.
- **Summary** (LLM, rolling): 2-4 short factual sentences, at most 300 characters, of what has been done in the session overall (project or
  host, goal, what was tried, what happened, where it stands). Each run gets the previous summary plus the activity since, and updates it.

### Transcripts
Each session's output is written as plain text to `~/.local/state/nt/transcripts/<created_unix>-<id>.txt` through a small
`vte`-based stripper (printable characters; `\n` flushes a line; `\r`, BS and erase-line edit the current line; other escapes are
dropped). While the alternate screen is active nothing is transcribed; a marker line `[<fg_cmd> in <cwd>]` is written instead.
Writes are buffered and flushed every few seconds. A file is capped at 4 MiB (when full it keeps the last 2 MiB). A `.meta` file
stores the name, name source, topic and summary. Transcripts feed search and the naming digest.

Debug aid: starting the daemon with `NT_NAMER_NOW=1` makes the naming loop tick every second and run a batch as soon as a session has
any new output, ignoring the batch interval, minimum-output and backoff limits. `contrib/demo/` uses it to have summaries ready for the
recording. Not documented for users.

## Shell integration
- At spawn nt injects a hook (config `shell_integration`, `--no-shell-integration`). The user's rc files are always sourced first and
  nothing on disk is modified. Hooks are embedded from `src/server/hooks/` and written to the runtime dir.
  - bash: `bash --rcfile bash-init -i` (not `-l`, which ignores `--rcfile`). `bash-init` replays login startup (/etc/profile, then
    the first of ~/.bash_profile, ~/.bash_login, ~/.profile), prepends `__nt_precmd` and appends `__nt_arm` to `PROMPT_COMMAND`
    (string or array) and chains any existing DEBUG trap; the trap acts only once armed, so PROMPT_COMMAND subcommands are ignored.
    PS1 is left alone, so no `OSC 133;B` is emitted.
  - zsh: `ZDOTDIR` points to a wrapper that restores the original `ZDOTDIR`, sources the user's files, then adds `precmd`/`preexec`.
  - fish: `--init-command`.
  - Other shells: no integration, heuristics only.
- The hooks emit OSC 133 (`A` prompt start, `C` output start, `D;<exit>` finished, `E;<cmd>` the command line, bytes outside printable
  ASCII, `;` and `\` escaped as `\xNN`) and OSC 7 (`file://host/cwd`).
- The server parses these from the PTY stream (via the vte stripper) into a per-session command log of the last 50 entries:
  `{cmd, cwd, start, end, exit, notable_lines}` where `notable_lines` is at most 3 error-looking lines plus the last non-empty
  output line. Without integration, commands are guessed from prompt-looking lines and the exit status is unknown.
- **Terminal titles** (OSC 0/2, BEL or ST, passed through to clients unchanged) are sanitized (controls dropped, 120 chars) and recorded
  per command (`title` = latest, `titles` = last 5 distinct) and per session (`title`, cleared when the command ends). Default shell titles
  are ignored: any title while the shell is foreground (after `D` until the next command starts; without integration `tcgetpgrp == shell`),
  `user@host: path`, the cwd (also `~`-abbreviated) and the command line itself. A leading decorative glyph/spinner (`✳ ⠂ · *`) is stripped, so
  glyph animation neither changes the title nor marks the session dirty; real text changes do, at most once per 10 s.
  Status: `⏵ claude · Fix reconnect bug in proto.rs · 12m` (alt screen: `✎ vim · README.md`). The stripper counts redraws (cursor up/home,
  erase, in-place overwrites); a command with >= 20 and more redraws than lines is TUI noise: its digest line says so and its output lines are
  dropped, leaving the title (`title: "…" (earlier: "…")`). Without an LLM the topic is the title.

## LLM naming
- **Digest** (input; the same format is used by `nt name-test` and the benchmark). The first run of a session lists its recent
  commands (at most 12, the newest); a later run is **rolling**: it starts with `earlier summary: <the stored summary>`, then lists only
  the commands newer than that summary (`CommandLog` numbers them; `Session.summary_seq` is the number the summary covers, minus a
  command that was still running, which is listed again with its result) under `new commands since then (oldest first):`. Extra lines
  `hosts:` (ssh destinations) and `files:` (arguments of editor commands) are derived from the listed commands. A session without
  new commands is not re-summarized. Without shell integration the guessed digest uses the transcript tail since the last run.
  Example of a rolling digest:
```
cwd: ~/src/nt (git nt, branch reconnect)
commands (oldest first):
  [~/src/nt] cargo build            ok      12s
  [~/src/nt] cargo test             exit 101 40s
      | error[E0308]: mismatched types --> src/proto.rs:88
  [~/src/nt] make deploy            running 3m
      | Uploading 45%
foreground: make deploy
```
```
earlier summary: Working on the nt reconnect logic in ~/src/nt. Fixed a type error in proto.rs after a failed cargo test.
cwd: ~/src/nt (git nt, branch reconnect)
new commands since then (oldest first):
  [~/src/nt] cargo test             ok      52s
  [~/src/nt] make deploy            running 3m
      | Uploading 45%
foreground: make deploy
```
- **Output**: three lines, `name: <2-4 lowercase kebab words>`, `topic: <= 50 chars>` and `summary: <2-4 short factual sentences, <= 300
  chars>`. Only the first `name:` line, the first `topic:` line after it and the first `summary:` line after that (plus wrapped
  continuation lines up to a blank line) are kept, then junk filters (`sanitize_name`, `sanitize_recap`, `sanitize_summary`; a summary
  that is too long is cut after the last full sentence that fits, a summary of fewer than 4 words is dropped). The daemon stores the
  summary and the new `summary_seq` only when a topic or summary came back.
- **Privacy of the digest**: before the digest goes to the local model or the API (`server/privacy.rs`; the previous summary is part of it) the
  home directory path becomes `~`, the login name (`$USER`, `$LOGNAME`, else the last component of `$HOME`) becomes `user` and the short host name
  becomes `host`. Names are replaced only as whole words / path components (neighbours are not letters, digits or `_`; user `ann` leaves
  `annotate` alone), in the exact and the lowercase form, and only if they have at least 3 characters. The prompt says these are placeholders to
  never interpret (a login name that looks like a word or acronym would otherwise end up in topics). Changing the prompt text changes the
  evaluated prefix tokens and with them the prefix-cache key, so the old cache file is replaced on the next start.
- **Prompt**: prompt v3, the one chosen by the benchmark (`src/server/prompt.rs`, `bench/prompts/v3.json`, generated by
  `bench/prompt_from_rs.py`): a system message with rules (including how to update an earlier summary and not to invent details) and five
  few-shot user/assistant turns (three first-run digests, two rolling updates), then the digest as the last user turn, rendered with the
  GGUF's own chat template (Qwen3-style templates get an empty think block). Greedy decoding, at most 130 new tokens, stop after the
  summary line.
- **Process model**: naming runs in a short-lived child, `nt namer`, never inside the daemon; the daemon sends it jobs on a line
  protocol and reads results back. The child runs at SCHED_IDLE, nice 19 and idle I/O priority. The daemon spawns it as `/proc/self/exe` (works after
  an upgrade replaced the binary on disk) and applies each result as soon as the child prints it. If a batch fails the daemon backs
  off for 5 minutes, keeps the error (`S2C::NamerError`, sent before `Sessions` in the reply to `List` and pushed on change) and the
  switcher title shows `naming: error, see server.log` while `nt ls` prints it. `SessionInfo.pending` marks sessions in the running batch (a dim braille spinner `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`, ASCII `|/-\`, after the topic, animated by the client at ~8 fps only while the switcher is open and a session is pending). When the resource guards skip a run, the server sends `S2C::NamerPaused(reason)` (`battery`, `low memory`, `memory pressure`) and the switcher title shows a dim `naming paused: <reason>`.
- **Scheduling**: a session is *dirty* after OSC 133 `D` (or a prompt ending a command), a cwd change (OSC 7), an `ssh` command line,
  or >= 1 KiB new transcript (`Session::dirty`, cleared when a job is built). **Opening the switcher refreshes every session with activity since its summary** (a command started or ended, including Ctrl-C'd ones, whose OSC 133 `D` carries
  the exit 130; cwd, branch or group change; or >= 256 B of new output, instead of 1 KiB), current session first, at once and without the per-session failure
  backoff; sessions that do not fit in the batch of 4 are shown pending too and follow in the next batch. If the resource guards postpone it, it is retried on
  every tick while the switcher stays open. Staleness is exact, not timed: a summary made while command N ran covers N-1; it is stale when a later command exists or N has ended.
  Background: a batch when some dirty session has had no output for 20 s,
  at most one batch per 2 min (measured from the last start). `Watch(true)` wakes the loop: a batch starts at once for dirty sessions with no cooldown (the resource guards still apply). If a batch is
  already running, or more sessions are due than fit in one batch (4), the loop runs a follow-up batch right after it ends while a client still watches.
  Job order in a watched batch: the watching client's current session first, then by last use (the switcher's order); results are applied and pushed per session as the child prints them. Pure rules: `namer::batch_allowed`, `session_ready`.
- **Prefix cache**: the evaluated prefix state (`state_seq_save_file`, any architecture) is stored in `$XDG_CACHE_HOME/nt/prefix-<key>.state`,
  key = blake3 of model path, size, prefix tokens and `n_ctx` (constant: prefix + 700 + 130 + 16, so it does not depend on the jobs). Other `prefix-*` files are
  deleted. Measured with Granite 1B on this machine (debug build, 2 threads, idle priority): first result after opening the switcher 26.4 s cold, 6.3 s warm; the state file is 121 MB.
- **Resource safety** (`src/server/guard.rs`, pure functions over /proc and /sys text, unit tested): before a local run `MemAvailable >= model x 1.3 + 300 MiB + 1 GiB`,
  `some avg10 < 5` and not on battery (config `llm_on_battery`, default false), else the run is postponed (logged once per episode, dirty flags stay). The child sets `oom_score_adj` 1000,
  loads weights with mmap, uses `llm_threads` (auto: nproc/2 <=16 for prompt, nproc/4 <=8 for generation), and is capped by `systemd-run --user --scope -p MemoryMax=<estimate+256 MiB> -p MemorySwapMax=0 -p CPUWeight=1`
  (probed once; it execs in place, started through `/proc/<daemon pid>/exe`), else `RLIMIT_DATA` = estimate + 512 MiB (private mappings only, file-backed weights are not counted).
  A watchdog thread in the daemon polls every second and SIGKILLs the child when `MemAvailable < max(5% of MemTotal, 512 MiB)` or after 180 s; that counts as a failed batch.
- **Local backend** (`llm = "local"`, cargo feature `llm`, on by default): llama.cpp through the `llama-cpp-2` crate, built from
  source by cmake (needs cmake, a C++ compiler and libclang; on Debian-like systems without clang also
  `BINDGEN_EXTRA_CLANG_ARGS=-I/usr/lib/gcc/x86_64-linux-gnu/<N>/include` for `stdbool.h`). OpenMP is off and libstdc++ is linked
  statically, so the binary depends on libc only. Tokenizer, vocabulary and chat template all come from the GGUF, so switching models
  is just `llm_model` (URL, downloaded once to `~/.local/share/nt/models/`, or a local path). The default build needs AVX2/FMA on
  x86-64 (ggml without runtime dispatch); a baseline build (`GGML_AVX2=OFF GGML_FMA=OFF GGML_F16C=OFF GGML_BMI2=OFF cargo build`) runs
  everywhere but is about 2.3x slower. The child uses `llm_threads` (auto: prompt clamp(nproc/2,1,16), generation clamp(nproc/4,1,8); llama-bench Granite 1B on 48 cores: pp512 t/s 1/2/4/8/16/24 threads = 75/147/270/454/800/1022, tg 30/42/45/56/57 at 1/2/4/8/16; a number N gives N and min(N,8); `available_parallelism` respects affinity and cgroup quotas) and evaluates the constant prompt prefix (~1500 tokens with Granite's tokenizer, ~17 s for Granite 1B and ~9 s for Qwen3.5-0.8B on 2 threads at idle priority) once (or loaded from the prefix cache); each job then starts from it by truncating the KV cache to the prefix length (attention models) or restoring a saved
  sequence state (recurrent/hybrid models such as Qwen3.5). The context is just prefix + longest digest + 130 tokens (~1900). Per session then: Granite 1B ~4.5-6 s, Qwen3.5-0.8B ~1.6-2.7 s (the reply is ~60-70 tokens).
- **Models**: default Granite 4.0 1B Instruct Q4_K_M (`ibm-granite/granite-4.0-1b-GGUF`, 1 GB file, ~1.8 GB peak RSS), the best in
  `bench/RESULTS.md`. Lighter alternative: Qwen3.5-0.8B Q4_K_M (`unsloth/Qwen3.5-0.8B-GGUF`, ~0.9 GB RSS), slightly lower quality.
- **API backend** (`llm = "api"`, works in builds without the `llm` feature): the same messages POSTed to `llm_api_url` (any
  OpenAI-compatible chat completions server) by `curl` inside `nt namer`. The URL, `Authorization` header and body go to curl as a
  config on stdin (`-K -`), so the key never appears in `ps`. 20 s timeout; the key comes from the daemon's environment variable
  `llm_api_key_env` or the file `llm_api_key_file`. Default model `gpt-5-nano`; names starting with `gpt-5` or `o<digit>` get
  `reasoning_effort: minimal` and `max_completion_tokens` (400), others `temperature: 0` and `max_tokens: 130`. The request JSON is built by
  hand (strings only) and the reply read by locating `choices[0].message.content`; this is the only JSON in the program, so there is
  no serde_json.
- **Privacy**: with `local` nothing leaves the machine. With `api` the digest (the earlier summary, cwd, repo, branch, last commands with status and a few
  output lines) is sent to the provider after `redact` masks `key=value` / `key: value` pairs whose key contains key, token, secret,
  passw, pwd, credential or auth, `Bearer` tokens, `--password x`-style flags, `user:pass@` in URLs, hex/base64-looking runs of 32+
  characters and well-known token prefixes (`sk-`, `ghp_`, `AKIA`, ...). This is best effort, not a guarantee; use `local` or `off`
  if commands or output may be sensitive. Without the `llm` feature or with `off`, only heuristic names are used.

## Config
`~/.config/nt/config.toml` (`$XDG_CONFIG_HOME`), read by a tiny hand-written reader (`key = "string" | number | bool`, `#` comments,
no toml crate; bad lines are reported and skipped; command-line flags win). Keys: `prefix = "C-]"`, `port`, `scrollback`,
`shell_integration`, `llm = "local" | "api" | "off"` (default `local` with the `llm` feature, else `off`), `llm_model`, `llm_api_url`,
`llm_api_model`, `llm_api_key_env`, `llm_api_key_file`, `llm_on_battery` (default false). Annotated example: `contrib/config.example.toml`.

## Dependencies
tokio (rt, net, io-util, sync, time, macros, signal), serde + postcard, nix, vt100, vte, crossterm, lexopt, anyhow, getrandom,
chacha20poly1305, blake3; optional feature `llm`: llama-cpp-2 (~6 MB of binary). The API backend uses the `curl` executable.

## Module layout
```
src/main.rs               CLI dispatch
src/proto.rs              messages, framing, crypto wrapper
src/config.rs             config reader
src/server/mod.rs         daemon, listeners, client handling
src/server/session.rs     pty, parser, ring buffer
src/server/shell.rs       shell hooks, OSC parsing, command log, digest, status
src/server/naming.rs      initial + heuristic names
src/server/transcript.rs  vte stripper, buffered writer, .meta, search
src/server/resources.rs   per-session CPU/RSS sampling
src/server/guard.rs       memory/pressure/battery gating and the kill watchdog rules
src/server/namer.rs       daemon-side batch runner + `nt namer` child + line protocol
src/server/llm.rs         local model (llama.cpp), used only by the child (feature `llm`)
src/server/api.rs         OpenAI-compatible backend (curl) + secret redaction
src/server/prompt.rs      prompt messages + reply parser
src/server/hooks/         embedded bash/zsh/fish hooks
src/client/mod.rs         connect/reconnect loop, passthrough, prefix handling
src/client/remote.rs      ssh bootstrap, TCP handshake
src/client/switcher.rs    overlay UI + line editor
src/client/usage.rs       CPU/RAM bars
bench/                    model benchmark (see bench/RESULTS.md)
```
