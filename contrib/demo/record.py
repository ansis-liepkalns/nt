#!/usr/bin/env python3
"""Record the nt demo (asciicast v2) in a throwaway environment.

    cargo build --release
    contrib/demo/record.py [--nt target/release/nt] [--out contrib/demo/demo.cast]
    agg --font-size 16 --theme dracula --fps-cap 12 --idle-time-limit 2 --speed 1.2 contrib/demo/demo.cast docs/demo.gif
    (or: contrib/demo/make-gif.sh)

Everything runs under a temporary HOME / XDG_RUNTIME_DIR / XDG_STATE_HOME / XDG_DATA_HOME with its own TCP port, so the
real nt daemon, its sessions and ~/.local/state/nt are never touched. The Granite model is symlinked from an existing local
copy (nothing is downloaded). The "projects" are made-up: `cargo` and the log writer are small stand-ins that
print realistic output, `make test` and `python train.py` are real commands running toy scripts. The daemon is started with
NT_NAMER_NOW=1 (debug aid, see DESIGN.md) so names, topics and summaries exist before the recorded part begins.
Stdlib only. Everything is stopped and deleted at the end.
"""
import argparse, codecs, fcntl, json, os, pty, random, re, select, shutil, signal, socket, struct
import subprocess, sys, tempfile, termios, time
from pathlib import Path

COLS, ROWS = 100, 30
FAKE_HOME_REAL, FAKE_HOME = "/tmp/hdev", "/home/dev"
assert len(FAKE_HOME_REAL) == len(FAKE_HOME)
OSC = re.compile(r"\x1b\][^\x07\x1b]*(\x07|\x1b\\)")
HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent

# ---------------------------------------------------------------------------------------------- sample content

BASHRC = r"""
export PATH="$HOME/bin:$PATH"
export HISTFILE=/dev/null
PS1='dev@box:\w$ '
"""

FAKE_CARGO = r"""#!/bin/bash
# Stand-in for cargo: prints what a small project build/test looks like.
case "$1" in
build)
  for c in "libc v0.2.155" "cfg-if v1.0.0" "rayon-core v1.12.1" "crossbeam-utils v0.8.20" "rayon v1.10.0" "glam v0.27.0" "image v0.25.1"; do
    echo "   Compiling $c"; sleep 0.2
  done
  echo "   Compiling raytracer v0.3.1 (/home/dev/src/raytracer)"; sleep 1.2
  echo "    Finished \`release\` profile [optimized] target(s) in 11.84s" ;;
test)
  echo "   Compiling raytracer v0.3.1 (/home/dev/src/raytracer)"; sleep 0.8
  echo "     Running unittests src/lib.rs (target/debug/deps/raytracer-3f9a1c)"; echo
  echo "running 6 tests"
  for t in vec3_dot sphere_hit plane_hit bvh_build bvh_traverse camera_rays; do
    sleep 0.25; echo "test geometry::$t ... ok"
  done
  echo; echo "test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s" ;;
*) echo "cargo $*" ;;
esac
"""

MAKEFILE = r"""test:
	@echo "pytest -q tests/"
	@sleep 0.6
	@echo ".............F.........                                                   [ 54%]"
	@echo "================================== FAILURES =================================="
	@echo "__________________ test_checkout_applies_discount ___________________"
	@echo ""
	@echo "    def test_checkout_applies_discount():"
	@echo "        cart = Cart([Item('widget', 1999)], coupon='SAVE10')"
	@echo ">       assert cart.total() == 1799"
	@echo "E       assert 1999 == 1799"
	@echo ""
	@echo "tests/test_cart.py:41: AssertionError"
	@echo "=========================== short test summary info ==========================="
	@echo "FAILED tests/test_cart.py::test_checkout_applies_discount - assert 1999 == 1799"
	@echo "1 failed, 41 passed in 2.31s"
	@false
"""

TRAIN_PY = r"""import time
print("loading dataset reviews.csv (25000 rows, 3 classes)")
time.sleep(1)
print("model: bert-tiny, 4.4M parameters, batch 64, lr 3e-4")
loss, acc = 1.0986, 0.33
weights = b"w" * (280 * 1024 * 1024)  # resident memory for the RAM bar
for epoch in range(1, 201):
    t = time.time()
    while time.time() - t < 1.0:  # "training": burn CPU for the CPU bar
        sum(i * i for i in range(2000))
    time.sleep(0.5)
    loss = loss * 0.93 + 0.012
    acc = min(0.93, acc + (0.93 - acc) * 0.09)
    print(f"epoch {epoch:3d}/200  loss {loss:.4f}  acc {acc:.4f}  val_acc {acc - 0.021:.4f}", flush=True)
"""

LOGGEN_PY = r"""import random, sys, time
random.seed(3)
paths = ["/api/cart", "/api/products", "/api/checkout", "/api/orders", "/healthz", "/api/search?q=lamp"]
msgs = [("INFO", "GET {p} 200 {ms}ms"), ("INFO", "POST {p} 201 {ms}ms"), ("WARN", "slow query on orders ({ms}ms)"),
        ("INFO", "payment-gateway: charge ok order={o}"), ("INFO", "refund issued order={o} amount=19.99"),
        ("ERROR", "payment-gateway: timeout after 3000ms order={o}")]
n = 0
with open(sys.argv[1], "a") as f:
    while True:
        n += 1
        lvl, m = random.choices(msgs, weights=[30, 12, 6, 8, 3, 2])[0]
        line = f"2025-03-14 09:{(n // 60) % 60:02d}:{n % 60:02d} {lvl:5s} " + m.format(p=random.choice(paths), ms=random.randint(3, 240), o=10000 + n)
        f.write(line + "\n"); f.flush()
        time.sleep(0.8)
"""

# ---------------------------------------------------------------------------------------------- pty driver

class Term:
    """A program on a pty of a fixed size; output is optionally written to an asciicast event list."""

    def __init__(self, argv, env, cwd):
        master, slave = os.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.proc = subprocess.Popen(
            argv, stdin=slave, stdout=slave, stderr=slave, env=env, cwd=cwd, start_new_session=True,
            preexec_fn=lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0),
        )
        os.close(slave)
        self.fd = master
        self.t0 = time.monotonic()
        self.events = None
        self.dec = codecs.getincrementaldecoder("utf-8")("replace")
        self.seen = ""

    def pump(self, secs):
        end = time.monotonic() + secs
        while True:
            left = end - time.monotonic()
            if left <= 0:
                return
            r, _, _ = select.select([self.fd], [], [], left)
            if r:
                try:
                    data = os.read(self.fd, 65536)
                except OSError:
                    return
                if not data:
                    return
                text = self.dec.decode(data)
                self.seen += text
                if self.events is not None and text:
                    self.events.append([round(time.monotonic() - self.t0, 4), "o", text])

    def wait_for(self, pattern, timeout=60):
        end = time.monotonic() + timeout
        mark = len(self.seen)
        while time.monotonic() < end:
            if re.search(pattern, self.seen[mark:]):
                return
            self.pump(0.1)
        raise TimeoutError(f"timed out waiting for {pattern!r}; tail: {self.seen[-300:]!r}")

    def send(self, data):
        os.write(self.fd, data if isinstance(data, bytes) else data.encode())

    def type(self, text, delay=0.085):
        """Human-ish typing: jittered delays, a longer pause after spaces."""
        for ch in text:
            self.send(ch)
            self.pump(random.uniform(0.6, 1.5) * delay * (1.6 if ch == " " else 1))

    def close(self):
        try:
            os.close(self.fd)
        except OSError:
            pass
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()

C_RB, C_N, C_P, C_S, C_U, RET = b"\x1d", b"\x0e", b"\x10", b"\x13", b"\x15", b"\r"

# ---------------------------------------------------------------------------------------------- environment

def find_model():
    name = "granite-4.0-1b-Q4_K_M.gguf"
    real_home = Path(os.path.expanduser("~"))
    for d in (real_home / ".cache/nt-bench/models", real_home / ".local/share/nt/models"):
        if (d / name).exists():
            return d / name
    sys.exit(f"{name} not found in ~/.cache/nt-bench/models or ~/.local/share/nt/models; run nt once to download it")

def write(path, text, mode=None):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    if mode:
        path.chmod(mode)

def build_home(root):
    home = root
    write(home / ".bashrc", BASHRC)
    write(home / ".bash_profile", "[ -f ~/.bashrc ] && . ~/.bashrc\n")
    write(home / "bin/cargo", FAKE_CARGO, 0o755)
    (home / "bin/python").symlink_to(shutil.which("python3"))
    # `nt devbox`: an ssh alias for this machine. ssh takes its config from the passwd home, not $HOME, so a wrapper in
    # PATH points it at the throwaway config. LogLevel ERROR keeps the "added to known hosts" notice out of the recording.
    write(home / "bin/ssh", '#!/bin/sh\nexec /usr/bin/ssh -F "$HOME/.ssh/config" -o BatchMode=yes "$@"\n', 0o755)
    (home / ".ssh").mkdir(mode=0o700)
    write(home / ".ssh/config", f"Host devbox\n  HostName 127.0.0.1\n  StrictHostKeyChecking accept-new\n"
                                f"  UserKnownHostsFile {home}/.ssh/known_hosts\n  LogLevel ERROR\n")
    port = socket.socket()
    port.bind(("127.0.0.1", 0))
    p = port.getsockname()[1]
    port.close()
    models = root / "data/nt/models"
    models.mkdir(parents=True)
    model = find_model()
    (models / model.name).symlink_to(model)
    # A local model path: nothing is downloaded, and the namer's command line names our private dir (see namer_running).
    write(home / ".config/nt/config.toml", f'port = {p}\nllm_model = "{models / model.name}"\n')
    for d in ("run", "state"):
        (root / d).mkdir(mode=0o700)
    (root / "run").chmod(0o700)
    genv = {"GIT_AUTHOR_NAME": "Dev", "GIT_AUTHOR_EMAIL": "dev@example.com", "GIT_COMMITTER_NAME": "Dev",
            "GIT_COMMITTER_EMAIL": "dev@example.com", "HOME": str(home), "PATH": os.environ["PATH"]}
    projects = {
        "webshop": {"Makefile": MAKEFILE, "README.md": "# webshop\nA small online shop: cart, checkout, orders.\n",
                    "frontend/package.json": '{\n  "name": "webshop-frontend",\n  "version": "0.4.0",\n  "scripts": {"dev": "vite", "build": "vite build"}\n}\n',
                    "frontend/src/App.jsx": "export default function App() { return <Cart/>; }\n",
                    "tests/test_cart.py": "def test_checkout_applies_discount():\n    pass\n", "logs/.keep": ""},
        "raytracer": {"Cargo.toml": '[package]\nname = "raytracer"\nversion = "0.3.1"\nedition = "2021"\n',
                      "src/lib.rs": "pub mod geometry;\n", "README.md": "# raytracer\nA toy path tracer.\n"},
        "ops": {"README.md": "# ops\nRunbooks and dashboards.\n"},
        "sentiment": {"train.py": TRAIN_PY, "README.md": "# sentiment\nReview sentiment classifier.\n"},
    }
    for name, files in projects.items():
        d = home / "src" / name
        for rel, text in files.items():
            write(d / rel, text)
        subprocess.run(["git", "init", "-q", "-b", "main"], cwd=d, env=genv, check=True)
        subprocess.run(["git", "add", "-A"], cwd=d, env=genv, check=True)
        subprocess.run(["git", "commit", "-q", "-m", "initial commit"], cwd=d, env=genv, check=True)
    write(root / "loggen.py", LOGGEN_PY)
    env = {
        "HOME": str(home), "XDG_RUNTIME_DIR": str(root / "run"), "XDG_STATE_HOME": str(root / "state"),
        "XDG_DATA_HOME": str(root / "data"), "XDG_CONFIG_HOME": str(home / ".config"),
        "PATH": f"{home}/bin:/usr/local/bin:/usr/bin:/bin", "SHELL": "/bin/bash", "TERM": "xterm-256color",
        "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8", "NT_NAMER_NOW": "1", "USER": "dev", "LOGNAME": "dev",
    }
    return home, env

def kill_everything(marker):
    """Stop every process that has our private XDG_RUNTIME_DIR in its environment (daemon, shells, scripts)."""
    for sig in (signal.SIGTERM, signal.SIGKILL):
        for p in Path("/proc").iterdir():
            if p.name.isdigit() and int(p.name) != os.getpid():
                try:
                    if marker in (p / "environ").read_bytes().decode(errors="replace"):
                        os.kill(int(p.name), sig)
                except OSError:
                    pass
        time.sleep(1)

# ---------------------------------------------------------------------------------------------- the show

def start_session(nt, env, home, setup):
    """Create a session with `nt`, run `setup` command lines in it, detach (C-] d)."""
    t = Term([nt], env, home)
    t.wait_for(r"dev@box")
    for line, wait in setup:
        t.send(line + "\r")
        t.wait_for(wait)
    t.pump(0.5)
    t.send(C_RB); t.pump(0.2); t.send(b"d")
    t.pump(0.5)
    t.close()

def namer_running():
    """Is an `nt namer` child of our private daemon alive? (the model call that writes topics and summaries)"""
    for p in Path("/proc").iterdir():
        if p.name.isdigit():
            try:
                if (lambda c: b"namer" in c[:3] and FAKE_HOME_REAL.encode() in b" ".join(c))((p / "cmdline").read_bytes().split(b"\0")):
                    return True
            except (OSError, IndexError):
                pass
    return False

def wait_for_namer(batches=3, timeout=420):
    """Wait until `batches` naming batches have finished. With NT_NAMER_NOW a batch starts as soon as a session has new
    output (one per session created, and again while the log and the training keep printing)."""
    end, done, was = time.time() + timeout, 0, False
    while time.time() < end:
        now = namer_running()
        if was and not now:
            done += 1
            if done >= batches:
                return
        was = now
        time.sleep(0.3)
    raise TimeoutError("namer did not run in time")

def has_recap(nt, env, status_re):
    """Does `nt ls` show a topic after the status of the session whose status matches `status_re`?"""
    out = subprocess.run([nt, "ls"], env=env, capture_output=True, text=True).stdout
    for l in out.splitlines():
        m = re.search(status_re + r"\s*(.*)$", l)
        if m:
            return bool(m.group(1).strip())
    return False

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--nt", default=str(ROOT / "target/release/nt"))
    ap.add_argument("--out", default=str(HERE / "demo.cast"))
    ap.add_argument("--keep", action="store_true", help="keep the temp dir")
    a = ap.parse_args()
    random.seed(11)
    # A fixed, short path with the same length as "/home/dev": the cast gets it replaced by "/home/dev" (same width, so
    # the switcher columns stay aligned). The sun_path limit for the socket also needs a short path.
    root = Path(FAKE_HOME_REAL)
    if root.exists():
        sys.exit(f"{root} exists; remove it first (leftover of an earlier run?)")
    root.mkdir(mode=0o700)
    marker = f"XDG_RUNTIME_DIR={root}/run"
    try:
        home, env = build_home(root)
        log = home / "src/webshop/logs/app.log"
        subprocess.Popen([sys.executable, str(root / "loggen.py"), str(log)], env=env, stdin=subprocess.DEVNULL,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        time.sleep(2)
        print("preparing sessions ...", flush=True)
        start_session(a.nt, env, home, [("cd ~/src/webshop", r"dev@box"), ("git status -s", r"dev@box"),
                                        ("head -4 Makefile", r"dev@box"), ("make test", r"Error 1"),
                                        ("echo '# TODO: coupon rounding' >> tests/test_cart.py", r"dev@box"),
                                        ("git diff --stat", r"dev@box"), ("make test", r"Error 1")])
        start_session(a.nt, env, home, [("cd ~/src/webshop", r"dev@box"), ("tail -f logs/app.log", r"refund issued")])
        start_session(a.nt, env, home, [("cd ~/src/webshop/frontend", r"dev@box"), ("less package.json", r"package.json")])
        start_session(a.nt, env, home, [("python -m http.server 8000", r"Serving HTTP")])
        start_session(a.nt, env, home, [("cd ~/src/sentiment", r"dev@box"), ("ls", r"dev@box"),
                                        ("head -3 train.py", r"dev@box"), ("git log --oneline", r"dev@box"),
                                        ("python train.py", r"epoch\s+8/")])
        print("waiting for the namer (first batch loads the model) ...", flush=True)
        wait_for_namer()
        time.sleep(25)  # let the rolling summaries catch up, and the log and the training advance a bit

        print("recording ...", flush=True)
        (home / "bin/nt").symlink_to(Path(a.nt).resolve())
        remote_env = " ".join(f"{k}={v}" for k, v in env.items() if k not in ("TERM",))
        outer = dict(env, PS1="dev@box:\\w$ ", NT_REMOTE_CMD=f"env {remote_env} {Path(a.nt).resolve()}")
        t = Term(["bash", "--norc", "--noprofile", "-i"], outer, home)
        t.events = []
        def clock():
            return time.monotonic() - t.t0
        t.pump(1.0)
        t.type("nt devbox"); t.pump(0.3); t.send(RET); t.pump(2.5)  # ssh bootstrap, then a new session (and the first-attach tip)
        t.type("cd ~/src/raytracer"); t.send(RET); t.pump(0.5)
        t.type("cargo build --release"); t.send(RET); t.pump(5.5)
        t.type("cargo test"); t.send(RET); t.pump(3.5)
        t.send(C_RB); t.pump(0.25); t.send(b"?"); t.pump(2.8)       # key help, from the console
        t.send(b" "); t.pump(0.6)                                   # any key closes it
        t.send(C_RB); t.pump(0.25); t.send(C_RB)                    # C-] C-]: the switcher, this session preselected
        t.pump(3.0)                                                 # spinner: its topic and summary are being written
        t_a = clock()
        while not has_recap(a.nt, env, r"cargo test · \d+[smhd] ago"):
            t.pump(0.5)
            if clock() - t_a > 120:
                raise TimeoutError("namer did not finish the new session")
        t.pump(0.5)
        t_b = clock()
        t.pump(3.5)                                                 # the updated topic and summary
        for key, wait in ((b"\x1b[B", 1.6), (b"\x1b[B", 1.6),      # down across the sessions of the groups
                          (b"\x1b[D", 2.2),                       # left: up to the group level
                          (b"\x1b[B", 1.4), (b"\x1b[B", 1.4),     # down between the group headers
                          (b"\x1b[C", 2.0)):                      # right: back into the group
            t.send(key); t.pump(wait)
        t.send(C_S); t.pump(0.8)                                    # full-text search over the transcripts
        t.type("refund", 0.15); t.pump(2.5)
        t.send(RET); t.pump(2.5)                                    # attach to the log session
        t.send(C_RB); t.pump(0.25); t.send(b"d"); t.pump(1.0)       # detach
        t.type("nt ls"); t.pump(0.3); t.send(RET); t.pump(3.0)
        events = t.events
        # Squeeze the wait for the namer (the spinner phase) into 1.5 s.
        span, keep = t_b - t_a, 1.5
        for e in events:
            if e[0] >= t_b:
                e[0] = round(e[0] - (span - keep), 4)
            elif e[0] > t_a:
                e[0] = round(t_a + (e[0] - t_a) * keep / span, 4)
        header = {"version": 2, "width": COLS, "height": ROWS, "timestamp": int(time.time()),
                  "env": {"TERM": "xterm-256color", "SHELL": "/bin/bash"}, "title": "nt demo"}
        t.send(b"exit\r"); t.pump(0.3)
        t.close()
        out = Path(a.out)
        with out.open("w") as f:
            f.write(json.dumps(header) + "\n")
            for e in events:
                # Strip OSC sequences (the shell's prompt marks and context reports carry the real host name) and
                # swap the temp home for a neutral path of the same length.
                e[2] = OSC.sub("", e[2]).replace(FAKE_HOME_REAL, FAKE_HOME)
                f.write(json.dumps(e, ensure_ascii=False) + "\n")
        text = out.read_text()
        for bad in (os.path.expanduser("~"), os.environ.get("USER", "\0"), socket.gethostname(), FAKE_HOME_REAL, "/tmp/"):
            if bad and bad in text:
                print(f"WARNING: cast contains {bad!r}", file=sys.stderr)
        print(f"wrote {out}: {len(events)} events, {events[-1][0]:.1f}s", flush=True)
    finally:
        if not a.keep:
            kill_everything(marker)
            shutil.rmtree(root, ignore_errors=True)

if __name__ == "__main__":
    main()
