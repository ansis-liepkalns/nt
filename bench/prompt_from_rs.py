#!/usr/bin/env python3
"""Write prompts/<name>.json from the SYSTEM / SHOTS constants of src/server/prompt.rs (the source of truth)."""
import json, os, re, sys
HERE = os.path.dirname(os.path.abspath(__file__))
src = open(f"{HERE}/../src/server/prompt.rs").read()

def lit(s):
    return json.loads('"' + s + '"')  # Rust and JSON string escapes agree for \n \" \\ used here

system = lit(re.search(r'pub const SYSTEM: &str = "((?:[^"\\]|\\.)*)";', src).group(1))
shots_src = src[src.index("pub const SHOTS"):src.index("];", src.index("pub const SHOTS"))]
strs = [lit(m) for m in re.findall(r'"((?:[^"\\]|\\.)*)"', shots_src)]
shots = [[strs[i], strs[i + 1]] for i in range(0, len(strs), 2)]
mx = int(re.search(r"MAX_NEW_TOKENS: usize = (\d+)", src).group(1))
name = sys.argv[1] if len(sys.argv) > 1 else "v3"
json.dump(dict(system=system, shots=shots, suffix="", max_tokens=mx), open(f"{HERE}/prompts/{name}.json", "w"), indent=1)
print(name, len(shots), "shots, max_tokens", mx)
