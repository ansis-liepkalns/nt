#!/usr/bin/env python3
"""Benchmark harness: llama-server per (model, threads), chat completions at temp 0, max 40 tokens."""
import json, os, re, subprocess, sys, time, glob, urllib.request, argparse, resource

HERE = os.path.dirname(os.path.abspath(__file__))
CACHE = os.path.expanduser("~/.cache/nt-bench")
SERVER = f"{CACHE}/build/bin/llama-server"

def server_pids(port):
    out = []
    for d in os.listdir("/proc"):
        if d.isdigit():
            try:
                c = open(f"/proc/{d}/cmdline").read().split("\0")
                if c and c[0].endswith("llama-server") and f"{port}" in c: out.append(int(d))
            except Exception:
                pass
    return out

def peak_rss(port):
    """Peak RSS (VmHWM, same as /usr/bin/time -v) of the server process listening on `port`."""
    best = 0
    for q in server_pids(port):
        st = open(f"/proc/{q}/status").read()
        best = max(best, int(re.search(r"VmHWM:\s+(\d+) kB", st).group(1)) / 1024)
    return best

CACHE_PROMPT = False

def load_prompt(name):
    return json.load(open(f"{HERE}/prompts/{name}.json"))

def scenarios():
    out = []
    for p in sorted(glob.glob(f"{HERE}/scenarios/*.txt")):
        lines = open(p).read().split("\n")
        gold = lines[0].removeprefix("# gold:").strip()
        name = os.path.basename(p)[:-4]
        # rolling scenarios: steps separated by "=== step N"; step 2+ get the model's previous summary prepended
        steps = re.split(r"^=== step \d+\n", "\n".join(lines[1:]), flags=re.M)
        if len(steps) == 1:
            out.append((name, gold, steps[0].strip()))
        else:
            out.append((name, gold, [s.strip() for s in steps]))
    return out

def messages(pr, digest):
    m = [{"role": "system", "content": pr["system"]}]
    for u, a in pr["shots"]:
        m += [{"role": "user", "content": u + pr["suffix"]}, {"role": "assistant", "content": a}]
    m.append({"role": "user", "content": digest + pr["suffix"]})
    return m

def post(port, body):
    r = urllib.request.Request(f"http://127.0.0.1:{port}/v1/chat/completions",
                               json.dumps(body).encode(), {"Content-Type": "application/json"})
    return json.load(urllib.request.urlopen(r, timeout=300))

def run_model(model, threads, pr, port):
    cmd = [SERVER, "-m", model, "--jinja", "-t", str(threads), "-tb", str(threads),
           "-c", "4096", "-ngl", "0", "--port", str(port), "--parallel", "1", "--no-webui", "--log-disable"]
    errf = open(f"/tmp/nt-bench-time-{port}.txt", "w")
    p = subprocess.Popen(cmd, stderr=errf, stdout=subprocess.DEVNULL)
    for _ in range(300):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=1); break
        except Exception:
            time.sleep(0.5)
    res = []
    try:
        for name, gold, digest in scenarios():
            steps = digest if isinstance(digest, list) else [digest]
            prev = ""
            for i, d in enumerate(steps):
                if i > 0 and prev:
                    d = f"earlier summary: {prev}\n" + d
                body = {"messages": messages(pr, d), "temperature": 0, "max_tokens": pr.get("max_tokens", 40),
                        "stop": ["\n\n"], "cache_prompt": CACHE_PROMPT,
                        "chat_template_kwargs": {"enable_thinking": False}}
                r = post(port, body)
                t = r.get("timings", {})
                raw = r["choices"][0]["message"]["content"].strip()
                m = re.search(r"^summary: (.*)$", raw, re.M)
                prev = m.group(1).strip() if m else ""
                res.append(dict(scenario=name if len(steps) == 1 else f"{name}#{i + 1}", gold=gold, out=raw, digest=d,
                                prompt_n=t.get("prompt_n"), prompt_ms=t.get("prompt_ms"),
                                gen_n=t.get("predicted_n"), gen_ms=t.get("predicted_ms")))
        # peak RSS (VmHWM, what /usr/bin/time -v reports) of the server and its children
        rss = peak_rss(port)
    finally:
        for q in server_pids(port): os.kill(q, 9)
        p.kill(); p.wait(); errf.close(); time.sleep(0.5)
    return res, rss

def words(s): return set(re.findall(r"[a-z0-9]+", s.lower()))

def clean(out):
    """What nt would keep: first `name:` line, the first `topic:` line after it, the first `summary:` line after that."""
    name = topic = summ = None
    for l in out.split("\n"):
        l = l.strip()
        if name is None and l.startswith("name:"): name = l
        elif name is not None and topic is None and l.startswith("topic:"): topic = l
        elif topic is not None and summ is None and l.startswith("summary:"): summ = l
    return "\n".join(x for x in (name, topic, summ) if x)

def fmt_ok(out):
    ls = [l for l in out.split("\n") if l.strip()]
    if len(ls) not in (2, 3): return False
    m = re.fullmatch(r"name: ([a-z0-9]+(?:-[a-z0-9]+){1,3})", ls[0].strip())
    t = re.fullmatch(r"topic: (.{3,50})", ls[1].strip())
    if len(ls) == 3:  # v3: a summary of 20..300 chars
        return bool(m and t and re.fullmatch(r"summary: (.{20,300})", ls[2].strip()))
    return bool(m and t)

def overlap(out, gold):
    out = "\n".join(l for l in out.split("\n") if not l.startswith("summary:"))
    g, o = words(gold) - {"name", "topic"}, words(out) - {"name", "topic"}
    return len(g & o) / len(g) if g else 0

if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--models", nargs="+", required=True)
    ap.add_argument("--prompt", default="v2")
    ap.add_argument("--threads", nargs="+", type=int, default=[1, 2])
    ap.add_argument("--out", required=True)
    ap.add_argument("--cache", action="store_true", help="reuse the KV cache of the constant prefix (system+few-shot)")
    a = ap.parse_args()
    CACHE_PROMPT = a.cache
    pr = load_prompt(a.prompt)
    allres = json.load(open(a.out)) if os.path.exists(a.out) else {}  # resumable
    port = 8700
    for mp in a.models:
        mp = mp if os.path.exists(mp) else f"{CACHE}/models/{mp}"
        key = os.path.basename(mp).removesuffix(".gguf")
        for th in a.threads:
            port += 1
            if f"{key}|{th}" in allres: continue
            try:
                res, rss = run_model(mp, th, pr, port)
            except Exception as e:
                print(f"{key} t={th} FAILED: {e}", flush=True); continue
            for r in res:
                r["raw"] = r["out"]; r["out"] = clean(r["raw"])
                r["fmt"] = fmt_ok(r["out"]); r["overlap"] = overlap(r["out"], r["gold"])
            allres[f"{key}|{th}"] = dict(model=key, threads=th, rss_mb=rss, size_mb=os.path.getsize(mp) / 1e6, results=res)
            n = len(res)
            print(f"{key} t={th} rss={rss:.0f}MB fmt={sum(r['fmt'] for r in res)}/{n} "
                  f"overlap={sum(r['overlap'] for r in res)/n:.2f} "
                  f"pp={sum(r['prompt_ms'] for r in res)/n:.0f}ms gen={sum(r['gen_ms'] for r in res)/n:.0f}ms", flush=True)
            json.dump(allres, open(a.out, "w"), indent=1)
