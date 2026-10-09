#!/usr/bin/env python3
"""Summary-aware report for prompt v3: python3 report_v3.py results/v3.json [--examples]"""
import json, re, sys, os
HERE = os.path.dirname(os.path.abspath(__file__))
K = json.load(open(f"{HERE}/keys.json"))

def summary_of(out):
    m = re.search(r"^summary: (.*)$", out, re.M)
    return m.group(1).strip() if m else ""

def unsupported(summary, digest):
    """Specific-looking tokens (digits, paths, identifiers, capitals) of the summary that do not occur in the digest."""
    d = digest.lower()
    bad = []
    for t in re.findall(r"[~/\w.@:\-]+", summary):
        t = t.strip(".,;:()'\"`-")
        if len(t) < 3: continue
        specific = bool(re.search(r"[\d/._~@-]", t)) or any(c.isupper() for c in t[1:])
        if specific and t.lower() not in d:
            bad.append(t)
    return bad

def kw(r):
    o = r["out"].lower()
    g = K[r["scenario"]]
    return sum(any(w in o for w in grp) for grp in g) / len(g)

d = json.load(open(sys.argv[1]))
print("| model | threads | fmt ok /%d | kw | mean new tokens | prompt ms | gen ms | total ms | summaries with unsupported tokens | RSS MB |" % len(next(iter(d.values()))["results"]))
print("|---|---|---|---|---|---|---|---|---|---|")
for k, v in sorted(d.items()):
    res = v["results"]; n = len(res)
    bad = [(r["scenario"], unsupported(summary_of(r["out"]), r["digest"] if "digest" in r else "")) for r in res]
    nbad = sum(1 for _, b in bad if b)
    pp = sum(r["prompt_ms"] for r in res[1:]) / (n - 1); gen = sum(r["gen_ms"] for r in res[1:]) / (n - 1)  # first request fills the cache
    print(f"| {v['model']} | {v['threads']} | {sum(r['fmt'] for r in res)} | {sum(kw(r) for r in res)/n:.2f} | {sum(r['gen_n'] for r in res)/n:.0f} | {pp:.0f} | {gen:.0f} | {pp+gen:.0f} | {nbad}/{n} | {v['rss_mb']:.0f} |")
    if "--bad" in sys.argv:
        for s, b in bad:
            if b: print(f"    {v['model']} {s}: {b}")
