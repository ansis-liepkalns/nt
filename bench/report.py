#!/usr/bin/env python3
"""Turn results/*.json into markdown tables (printed to stdout)."""
import json, sys, re, os
HERE = os.path.dirname(os.path.abspath(__file__))
K = json.load(open(f"{HERE}/keys.json"))

def score(r):
    """Keyword score 0..1 (fraction of concept groups hit) + format. Crude proxy; read the outputs too."""
    o = r["out"].lower()
    groups = K[r["scenario"]]
    hit = sum(any(w in o for w in g) for g in groups) / len(groups)
    return hit

def distinct(res):
    d = {r["scenario"]: r["out"] for r in res}
    return d["14a-frontend-dev"].split("\n")[0] != d["14b-db-migration"].split("\n")[0] and \
        d["14a-frontend-dev"] != d["14b-db-migration"]

def summarize(path):
    d = json.load(open(path)); rows = {}
    for v in d.values():
        res = v["results"]; n = len(res)
        m = rows.setdefault(v["model"], dict(size=v["size_mb"]))
        m[f"rss{v['threads']}"] = v["rss_mb"]
        m[f"pp{v['threads']}"] = sum(r["prompt_ms"] for r in res) / n
        m[f"gen{v['threads']}"] = sum(r["gen_ms"] for r in res) / n
        m[f"gn{v['threads']}"] = sum(r["gen_n"] for r in res) / n
        m["pn"] = sum(r["prompt_n"] for r in res) / n
        m["kw"] = sum(score(r) for r in res) / n
        m["fmt"] = sum(r["fmt"] for r in res)
        m["split"] = distinct(res)
    return d, rows

if __name__ == "__main__":
    d, rows = summarize(sys.argv[1])
    print("| model | GGUF MB | kw score | fmt ok /15 | 14a/b distinct | pp ms (1t) | gen ms (1t) | pp ms (2t) | gen ms (2t) | RSS MB (1t) |")
    print("|---|---|---|---|---|---|---|---|---|---|")
    for k, m in sorted(rows.items(), key=lambda kv: -kv[1]["kw"]):
        print(f"| {k} | {m['size']:.0f} | {m['kw']:.2f} | {m['fmt']} | {'yes' if m['split'] else 'no'} | {m.get('pp1',0):.0f} | {m.get('gen1',0):.0f} | {m.get('pp2',0):.0f} | {m.get('gen2',0):.0f} | {m.get('rss1',0):.0f} |")
    print()
    models = sorted({v["model"] for v in d.values()})
    scen = [r["scenario"] for r in next(iter(d.values()))["results"]]
    gold = {r["scenario"]: r["gold"] for r in next(iter(d.values()))["results"]}
    out = {v["model"]: {r["scenario"]: r["out"] for r in v["results"]} for v in d.values() if v["threads"] == 1 or True}
    for s in scen:
        print(f"**{s}** — gold: `{gold[s]}`\n")
        for m in models:
            print(f"- {m}: `{out[m][s].replace(chr(10), ' / ')}`")
        print()
