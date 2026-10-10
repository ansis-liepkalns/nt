# nt

![license: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)
![built with Rust](https://img.shields.io/badge/built%20with-Rust-orange?logo=rust)
![server: Linux](https://img.shields.io/badge/server-Linux-lightgrey)

**Remote terminal sessions that survive anything — auto-reconnect, a built-in session switcher, live status and AI-written summaries. Rust, no tmux needed.**

![nt demo: session switcher with status, CPU/RAM bars, summaries and full-text search](docs/demo.gif)

`nt` is a reconnectable remote terminal with a built-in session switcher. Sessions live in a per-user daemon, so they survive
network loss, laptop sleep and roaming; the client reconnects on its own and picks up where it left off.

- **Reconnects by itself**: close the lid, change networks, come back; your shell and its output are still there.
- **Session switcher**: list, switch, rename, close and full-text search sessions from inside nt, each with a live status
  (`✓ cargo build · 2m ago`), CPU/RAM bars, a short topic and a searchable summary of what was done.
- **Multiple clients** can attach to one session.
- **Native scrollback**: output goes to your terminal emulator as is, so its scrollbar, search and selection keep working.
- **No panes or windows**: your terminal emulator does that. Emacs-style keys; works in an 80x24 terminal.

## Why nt?

| | plain ssh | tmux / screen | mosh | Eternal Terminal | **nt** |
|---|---|---|---|---|---|
| Survives network loss / sleep | no | session yes, you re-ssh and reattach | yes | yes | yes |
| Reconnects automatically | no | no | yes | yes | yes |
| Session list and switching | no | yes (panes, windows) | no | no | yes (switcher, no panes) |
| Several clients on one session | no | yes | no | no | yes |
| Native terminal scrollback | yes | no (own copy mode) | no | yes | yes |
| Status, summaries, full-text search | no | no | no | no | yes |
| Local echo prediction on bad links | no | no | yes | no | no |
| Transport | ssh | ssh | UDP | TCP | TCP (ssh bootstraps the connection) |

nt is not a tmux replacement if you want panes and scripting, and it does not do mosh's local echo or UDP roaming. Its server
runs on Linux only. If you mostly want "my sessions survive and I can find the right one", it is one binary and one command.

## Requirements
- Server: Linux. Client: Linux or macOS (best effort). Both need an `ssh` client for remote use, and `curl` is used for the
  model download and the API backend.
- Build: Rust **1.87 or newer**. Distro packages (e.g. `apt install rustc` on older Ubuntu) are often too old; use
  [rustup](https://rustup.rs) (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`, or `rustup update stable`).
- Local LLM feature (default): `cmake`, a C/C++ compiler and `libclang`. If libclang is installed without clang's own headers
  (common on Debian/Ubuntu: `stdbool.h` not found), either install `clang`, or let `install.sh` point bindgen at gcc's headers
  automatically. For a manual `cargo build`, set `BINDGEN_EXTRA_CLANG_ARGS="-I$(gcc -print-file-name=include)"`. On x86-64 the model code
  needs a CPU with AVX2/FMA.
- `--no-default-features` builds a slim pure-Rust binary: names come from heuristics, or from the API backend if you configure it
  (`llm = "api"`), or are off.

## Install
```
./install.sh                         # release build, installs to ~/.local/bin/nt
./install.sh --slim                  # without the local LLM: ~1.7 MB, pure Rust (alias --no-llm)
./install.sh --prefix /usr/local     # installs to /usr/local/bin/nt
./install.sh --remote me@server      # also copies the binary to server:~/.local/bin/nt (same OS and CPU architecture)
```
Or by hand: `cargo install --path .` (add `--no-default-features` for the slim build).

**Which build where.** The LLM only runs on the server, in the daemon's background namer; clients just display what the
server sends.

| machine | build | needs |
|---|---|---|
| laptop / PC you connect **from** | `./install.sh --slim` (~1.7 MB, pure Rust) | Rust only |
| server that **runs** sessions, with AI summaries | `./install.sh` (~7 MB) | Rust, cmake, C/C++ compiler, libclang |
| server without AI summaries | `./install.sh --slim` | Rust only (names from repo/command, or `llm = "api"`) |

## Quick start
```
nt                   # start a new session locally; the daemon starts on demand
nt myserver          # start a new session on myserver: ssh bootstraps the daemon, then nt connects directly over TCP
nt -s [myserver]     # open the session list (with no sessions, a new one is started)
nt ls [myserver]     # print sessions grouped by project folder, one line each: id, name, status, topic
nt restart [myserver]  # restart the nt server after an upgrade (ends its sessions)
nt --help | --version
```
On the server, nt is looked up on `PATH` plus `~/.local/bin` and `~/.cargo/bin` (set `NT_REMOTE_CMD=/path/to/nt` on the client for other locations), and TCP
port **4022** must be reachable (config `port`). `nt` always starts a new session; to get back to an existing one use `nt -s`
(the most recently used session is preselected) or, inside nt, `C-] C-]`. Extra ssh arguments go after `--`: `nt myserver -- -p 2200 -J jump`.

## Key bindings
Prefix is `C-]` (config `prefix`). Everything else goes straight to the shell.

| keys | action |
|---|---|
| `C-] C-]` or `C-] s` | session switcher |
| `C-] n` / `C-] p` | next / previous session |
| `C-] l` | last session |
| `C-] c` | new session |
| `C-] r` | rename current session (empty name = automatic) |
| `C-] k` | close current session (y/n; an exited session closes at once) |
| `C-] d` | detach (exit the client, session keeps running); a dim tip with the main keys is shown for 3 s on first attach (config `tips = false` disables it) |
| `C-] ?` | show the key list (any key closes it; also works inside the switcher) |
| `C-] q` | quit the client (also while reconnecting) |
| `C-] ]` | send a literal `C-]` |

Sessions are grouped by folder tree. A session's project is the outermost directory above its shell's working directory that holds a
`.git` or a project file (`Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, `Makefile`, `CMakeLists.txt`, `.hg`), looked for
only below `$HOME`; sessions in nested repos join the outer project. Sessions outside any project are grouped when their directories are
nested (`/etc/nginx` and `/etc/nginx/conf.d`), never under `$HOME`, `/`, `/tmp` or a top-level directory. A group shows a header with
the folder, the status icons of its sessions (worst first) and the summed CPU/RAM, its sessions are indented below it with the
subdirectory in place of the working directory, and ungrouped sessions come last. Groups follow the working directory, so `cd` moves a
session between them. `nt ls` prints the same grouping. Typing a group's name in the filter shows the whole group.

The list works like alt-tab: the current session is first (marked `*`), then the others by most recent use (attach or typing),
sessions whose shell has exited last. The current session is preselected (`RET` just closes the switcher); `C-] l` jumps to the last session you were in. The order is fixed when the switcher opens: live updates (status, topic, CPU) change what is shown, never where it is,
and new sessions are added at the end. The selected entry has an accent `▌` marker in the left column (`>` with `NT_ASCII=1`) and a
bold name; other entries' topics are dim. Typing filters (best match first, recency breaks ties): matches id, name, command, cwd, topic and summary, then

| keys | action |
|---|---|
| `C-n` `C-p` / `↑` `↓` | move over the sessions (across groups); `C-v` `M-v` / PgDn PgUp page; `M-<` `M->` first / last |
| `←` / `→` | `←` moves up to the group level (`↑` `↓` then move between groups; an ungrouped session is its own entry), `→` goes back into the selected group, onto its most recently used session. `RET` on a group attaches to that session. The filter cursor keeps `C-b` `C-f` |
| `RET` | attach |
| `C-o` or `M-RET` | new session |
| `M-r` | rename (RET confirm, C-g cancel, empty = automatic name) |
| `C-k` | close the selected session (y/n; at once if its shell has exited) when the cursor is at the end of the filter text, otherwise kill-line. `M-k` always closes |
| `M-i` | show / hide the summary of the selected session under the list (needs a terminal of 20+ rows) |
| `M-s` | cycle sort: recent use (default), CPU, RAM |
| `C-s` | toggle full-text search over session transcripts and summaries |
| `C-g` / `ESC` | cancel |
| `C-] d` or `C-x C-c` | detach (exit the client; the session keeps running) |

The filter line takes `C-a C-e C-b C-f C-d C-h M-b M-f C-w M-d C-k C-u` (`C-k` as above). When a shell exits the session stays, showing
`shell exited · RET new shell · C-k close` on the bottom row: `RET` starts a new shell in the same directory, `C-k` (or `M-k`) closes the session
at once, without y/n (the exit was the first step). Set `NT_ASCII=1` for ASCII-only drawing.

## Shell integration
nt starts bash, zsh and fish sessions with a small hook (OSC 133 / OSC 7 prompt marks) that reports commands, exit codes and the
working directory. It gives the live status line and the command log used for naming. Your rc files are sourced first and nothing
on disk is modified. Other shells work without it, with heuristic status. Turn it off with `shell_integration = false` or
`nt server --no-shell-integration`.

## Session names, status, topic and summary
Each session has a **name** (2-4 words, e.g. `webshop-api`), a deterministic live **status** (`⏵ make -j8 · 3m12s`, `✓ …`,
`✗ cargo test (101) · 5s ago`), an LLM-written **topic** (the goal, up to 50 characters) and an LLM-written rolling **summary**
(2-4 factual sentences, up to 300 characters, of what has been done in the session so far: project or host, goal, what was tried,
where it stands). Programs that set a terminal title (AI coding agents, editors) show it in the status (`⏵ claude · Fix reconnect bug · 12m`) and the namer prefers it over their noisy redrawn output. The summary is updated from the previous summary plus the activity since the last run, so it keeps older context.
The switcher shows the selected session's summary under the list (`M-i` toggles), the filter and `C-s` search look into it, and
the full text is searchable there. Names start random, then follow the
foreground command and git repo or directory; the LLM refines name and topic. A name you set yourself is never changed.

- **Local model** (default, `llm = "local"`): a small model run by llama.cpp in a short-lived `nt namer` child process at idle
  CPU/IO priority, only for sessions with recent activity. Default: Granite 4.0 1B Q4_K_M, a 1 GB download on first use into
  `~/.local/share/nt/models/`, about 1.8 GB RAM while it runs, about 5 s per session; the evaluated prompt prefix is cached in `~/.cache/nt/`, so a batch starts in a few seconds instead of ~17 s on the first run. Lighter alternative with about half the
  RAM and slightly lower quality: Qwen3.5-0.8B (`llm_model = "https://huggingface.co/unsloth/Qwen3.5-0.8B-GGUF/resolve/main/Qwen3.5-0.8B-Q4_K_M.gguf"`).
  Any GGUF that llama.cpp supports works, as a URL or a local path. See `bench/RESULTS.md` for how these were chosen.
- **API backend** (`llm = "api"`): any OpenAI-compatible chat completions endpoint (`llm_api_url`, `llm_api_model`, key from the
  environment variable `llm_api_key_env` or the file `llm_api_key_file`). Needs an API key with billing; a ChatGPT subscription
  does not include API access. The command digest (cwd, repo, branch, recent commands with status and a few output lines) is sent
  to the provider after masking likely secrets (`key=value` pairs with secret-looking keys, bearer tokens, `--password` flags,
  URL credentials, long hex/base64 runs, well-known token prefixes). Masking is best effort. Use `local` or `off` if your
  terminals may show sensitive data. Whatever the backend, the digest never contains your home directory path (shown as `~`), your
  login name (`user`) or the short host name (`host`).
- **`llm = "off"`**: heuristic names and no topic; the status line still works.

### When it runs
A session becomes *dirty* when a command ends, its directory changes, you `ssh` somewhere, or it wrote 1 KiB of new output. In the
background a batch runs when a session is dirty and has been quiet for 20 s, at most one batch per 2 minutes. Opening the switcher
runs a batch at once if anything is dirty (unless the last one ended less than 30 s ago); results appear in the open switcher as
they arrive, and a dim `…` after a topic marks a session being updated. If naming fails, the switcher title shows
`naming: error, see server.log` and `nt ls` prints the error; it retries after 5 minutes.

### Resource use
The local model is built not to bother you or the machine:
- CPU: idle scheduling class, nice 19, idle I/O class. Threads (`llm_threads = "auto"`): prompt processing cores/2 (at most 16), generation cores/4 (at most 8),
  from llama-bench measurements; a number N means N and min(N, 8). The cgroup scope sets only `CPUWeight=1` (no quota), so idle
  cores are fully used and anything else that wants the CPU wins.
- Memory: the weights are memory-mapped (file-backed, reclaimable). A run starts only if `MemAvailable` covers the model
  (file size x 1.3 + 300 MiB) plus a 1 GiB margin and memory pressure (`/proc/pressure/memory`, `some avg10`) is below 5 (below 25 when at least 3x the needed memory is free, so a busy but roomy machine still gets summaries);
  otherwise it is postponed and retried (logged once). The child has `oom_score_adj` 1000 (the kernel kills it first) and a hard
  cap: it runs in a `systemd-run --user --scope -p MemoryMax=... -p CPUWeight=1` scope when that works, else under `RLIMIT_DATA`.
  The daemon polls every second and kills the child if `MemAvailable` falls below 5% of RAM (at least 512 MiB) or it runs over 180 s.
- Battery: on battery power (`/sys/class/power_supply`, a Mains supply that is offline) nothing runs unless `llm_on_battery = true`.

## Upgrading
Install the new binary (`./install.sh`, plus `--remote` for servers), then run `nt restart` (or `nt restart myserver`) for each machine that runs a daemon:
it asks the old daemon to exit and starts the new one. **This ends the sessions held by the old daemon.** A client and daemon with
different protocol versions refuse to talk: the client prints `nt version mismatch …` and exits instead of retrying, and
`nt bootstrap` and the local autostart tell you that a different nt version is running with N sessions. Finish or save what is
running before restarting.

## Configuration
`~/.config/nt/config.toml` (or `$XDG_CONFIG_HOME/nt/config.toml`), simple `key = value` lines. All keys are optional; see
[`contrib/config.example.toml`](contrib/config.example.toml) for an annotated example. Command-line flags win over the file.

## Files nt writes
- Runtime: `$XDG_RUNTIME_DIR/nt/` (or `/tmp/nt-$UID/`, mode 0700): control socket, `server.log`, shell hooks.
- `~/.local/state/nt/transcripts/`: plain-text transcript per session (used by search; capped at 4 MiB each).
- `~/.local/share/nt/models/`: downloaded model.
- `~/.cache/nt/`: the evaluated prompt prefix of the model (~120 MB for Granite; safe to delete).
- Client side: nothing besides what ssh does.

## Security model
- Remote access starts with ssh: the bootstrap command, run over ssh, hands the client a random per-client key, so whoever can ssh
  in as you can connect, and nobody else.
- The TCP stream is authenticated and encrypted (ChaCha20-Poly1305, keys derived with BLAKE3 from that key and fresh nonces); unknown
  clients and tampered frames are dropped.
- The local control socket is plaintext but only reachable through a 0700 directory. The daemon runs as your user; nt does no
  privilege handling.

## Demo
The GIF is recorded by [`contrib/demo/record.py`](contrib/demo/record.py) in a throwaway environment with made-up projects and stand-in
commands (`contrib/demo/make-gif.sh` renders it with [agg](https://github.com/asciinema/agg)).

Inspired by mosh and Eternal Terminal.

## License
Apache-2.0, see [LICENSE](LICENSE).
