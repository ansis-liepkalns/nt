# Model benchmark for session name + topic

Task: from a command digest (format in `src/server/prompt.rs` / `../DESIGN.md`), produce
`name: <2-4 kebab words>` and `topic: <=50 chars`.

## Method
- 15 digests in `scenarios/` (14 scenarios; #14 is a pair of sessions in the same repo, frontend vs db), each with a `# gold:` line.
- 48-core x86 desktop, CPU only, llama.cpp built from source (Release, `-ngl 0`), `llama-server --jinja` (the model's own chat
  template), temperature 0, max 40 new tokens, stop at a blank line, Qwen3/3.5 with thinking disabled.
- Prompt `prompts/v2.json`: a system prompt with rules (name the project/host/task from cwd, repo, branch or host, not the last
  command; the topic is the work, not the status; do not invent; tell same-repo sessions apart) plus 3 few-shot turns on unrelated
  topics. It beat a terse 4-line prompt (v1) and a minimal 2-shot prompt with a per-turn suffix (v3) on the three strongest models
  (v3 hurt format reliability badly).
- Post-processing as in nt: keep the first `name:` line and the first following `topic:` line.
- Scores: `kw` = fraction of expected concept groups (`keys.json`) present in name+topic, a crude proxy; `fmt` = output is exactly
  a valid name line and a topic of 3-50 chars; `14a/b` = the two same-repo sessions get different names. Latency is the mean per
  request over the 15 digests, prompt caching off. RSS is the peak resident set of the server with a 4096 context.

## Candidates
SmolLM2 135M (baseline) / 360M, Qwen3 0.6B / 1.7B, Qwen3.5 0.8B, Gemma 3 270M / 1B, LFM2.5 230M / 350M / 1.2B, Granite 4.0
h-350m / 1B, Llama 3.2 1B. Q4_K_M everywhere, plus Q8_0 for models up to 400M. Larger or newer models over the ~2 GB RAM budget
were skipped.

## Summary

| model | GGUF MB | kw | fmt /15 | 14a/b | 1 thread ms (pp+gen) | 2 threads ms (pp+gen) | RSS MB |
|---|---|---|---|---|---|---|---|
| Qwen3-1.7B Q4_K_M | 1107 | 1.00 | 13 | yes | 10853 | 5679 | 2268 |
| **Granite 4.0 1B Q4_K_M** | 1024 | 1.00 | 15 | yes | 11371 | 5631 | 2056 |
| Qwen3.5-0.8B Q4_K_M | 533 | 0.97 | 11 | yes | 4752 | 2633 | 967 |
| LFM2.5-1.2B Q4_K_M | 731 | 0.93 | 12 | no | 7778 | 4041 | 1333 |
| Qwen3-0.6B Q4_K_M | 397 | 0.93 | 9 | no | 4431 | 2366 | 1128 |
| Llama-3.2-1B Q4_K_M | 808 | 0.90 | 10 | yes | 7105 | 3692 | 1479 |
| gemma-3-1b Q4_K_M | 806 | 0.90 | 15 | yes | 8300 | 4359 | 1035 |
| Granite 4.0 h-350m Q8_0 | 366 | 0.83 | 13 | no | 3137 | 1694 | 559 |
| SmolLM2-135M Q8_0 (baseline) | 145 | 0.70 | 12 | yes | 1813 | 999 | 288 |

Models below 400M other than Granite h-350m (LFM2.5-230M/350M, Gemma 3 270M, SmolLM2-360M) were worse still; the full run
(`run.sh`) prints all 19 variants.

The daemon evaluates the constant system+few-shot prefix (~610-650 tokens) once and reuses its KV state, so only the ~170-token digest
is processed per request. Mean per request with that cache, 1 / 2 threads: Granite 1B 3.2 / 1.7 s, Qwen3-1.7B 3.2 / 1.8 s,
Qwen3.5-0.8B 1.4 / 0.9 s, Granite h-350m 0.9 / 0.5 s. Quality is unchanged by caching.

## Ranking and recommendation
1. **Granite 4.0 1B Q4_K_M** (1.0 GB file, ~2 GB RSS): best outputs, 15/15 valid format, topics describe the work. nt's default.
2. **Qwen3-1.7B Q4_K_M**: essentially tied in quality, 13/15 format, slightly larger.
3. **Qwen3.5-0.8B Q4_K_M** (0.53 GB, ~1 GB RSS, under 1 s per request cached): best size/quality point, half the cost of 1-2.
   Weakness: topics sometimes describe status ("build failed") and it once mislabeled a repo. The documented lighter alternative.
4. Granite 4.0 h-350m Q8_0: usable but vague; the Q4 build is worse on format.
5. Qwen3-0.6B, Gemma 3 1B (clean format but vague or wrong), Llama 3.2 1B, LFM2.5-1.2B (cannot reuse the prompt cache; fails 14a/b).
6. Everything else under 400M copies few-shot names or invents them and rarely produces valid format.

## Example outputs (2 threads)

| scenario | gold | Granite 4.0 1B | Qwen3.5-0.8B |
|---|---|---|---|
| editor-config | webshop-config / webshop api: editing config after build | webshop-config / webshop: editing production config | webshop-frontend / vim production config file editing |
| build-failing | tokio-server-fix / tokio chat server: fixing borrow checker errors | chatd-async / chatd: async rewrite, hub errors | chatd-build / chatd: compilation error in hub.rs after cargo check |
| ssh-apt | nas-apt-upgrade / upgrading packages on the nas home server | nas01-upgrade / upgrading nas01 to latest packages | nas-apt-upgrade / system update running on NAS host |
| kernel-module | nvme-module-build / out-of-tree nvme kernel module: build and load | nvme-stats-driver / nvme driver mismatch, kernel version | nvme-stats-build / nvme_stats build failed due to kernel module format mismatch |
| latex | diffusion-paper / LaTeX paper on diffusion sampling: fixing bibliography | diffusion-sampling / latex: resolving bibliography compatibility issue | diff-sampling / LaTeX bibliography generation failed |
| db-migration | acme-db-migration / acme backend: running postgres schema migration | acme-db-upgrade / upgrading acme database to include tenant_id | acme-db-migration / database migration failed due to duplicate column |

## Prompt v3: rolling summary (`prompts/v3.json`)
The reply grew from two lines to three: `name`, `topic` (the goal, not the last command) and `summary` (2-4 factual sentences, at most 300
characters, of what was done in the session overall). The prompt (`src/server/prompt.rs`, exported by `prompt_from_rs.py`) is a system
message plus five few-shot turns: three first-run digests and two rolling updates. A rolling digest starts with `earlier summary: ...`
and lists only `new commands since then`; the model is told to condense the old text, let the newest commands decide where things
stand, and not to invent anything. (The "v3" of the first round, a terse per-turn-suffix prompt, was discarded and is not this one.)
Three iterations were run on Granite 1B and Qwen3.5-0.8B; the last one changed the digest wording and added the "newest commands decide"
rule, which fixed Granite copying a stale error into the updated summary.

Scenarios: the 15 digests above (first run) plus three two-step rolling ones (`15-17-rolling-*`: step 1 digest -> the model's own
summary -> step 2 = `earlier summary` + new activity), 21 requests per model. Greedy, max 130 new tokens, prompt cache on
(the prefix of ~1500 tokens is evaluated once; the first request of a run is left out of the latency means).
`report_v3.py` also lists summary tokens that look specific (numbers, paths, identifiers) but do not occur in the digest.

| model | threads | fmt ok /21 | kw | mean new tokens | prompt ms | gen ms | per session ms | summaries with unsupported tokens | RSS MB |
|---|---|---|---|---|---|---|---|---|---|
| Granite 4.0 1B Q4_K_M | 1 | 18 | 1.00 | 69 | 2588 | 5127 | 7715 | 3/21 | 2061 |
| Granite 4.0 1B Q4_K_M | 2 | 17 | 1.00 | 69 | 1324 | 3176 | 4500 | 2/21 | 2061 |
| Qwen3.5-0.8B Q4_K_M | 1 | 20 | 0.97 | 62 | 973 | 1550 | 2522 | 2/21 | 973 |
| Qwen3.5-0.8B Q4_K_M | 2 | 18 | 0.98 | 61 | 530 | 1109 | 1639 | 1/21 | 976 |

`fmt` here is strict (a 3-line reply, summary 20-300 chars, kebab name); nt's sanitizers repair most misses (a dotted name, a
too-long summary is cut at a sentence). The longest reply was 96 tokens, so 130 is enough. A summary costs about 60 more generated
tokens than the old topic-only reply, i.e. a session now takes ~4.5 s (Granite) or ~1.6 s (Qwen) on 2 threads, plus a one-time prefix
evaluation per `nt namer` batch (17 s Granite, 9 s Qwen at idle priority; `nt name-test` with release build).

**Granite 1B is clearly better than Qwen3.5-0.8B at this task**, so the default stays Granite. Granite's summaries cover the whole
session in order, use the cwd/branch, and its rolling updates fold the new commands in. Qwen's are shorter and less reliable:
it sometimes ignores the earlier summary and returns it unchanged (16 step 2 below), invents context ("billing database" in an acme
session, "Released zstd" for a build), and writes topics that are status ("zsh rebase failed due to conflicts"). Both can still be wrong
about the outcome: Granite 17 step 2 says the training run failed with the OOM although the digest shows it running at epoch 7;
Granite 05 says the conflict "was resolved" when the digest only shows vim being opened; Granite 06 invents the path `~/nas01`.

Example outputs (2 threads, verbatim):

| scenario | Granite 4.0 1B | Qwen3.5-0.8B |
|---|---|---|
| 03 build failing | chatd-async-rewrite / rewriting async code in chatd / Rewriting async code in chatd (~/code/chatd, branch async-rewrite). Cargo check failed with a borrow issue on `self.clients` and a missing `broadcast` method. Editing hub.rs to fix the issue. Cargo check failed again, so the rewrite is still in progress. | chatd-compiler / compiler error in chatd binary / Compiler failed to compile chatd due to borrow checker errors (cannot borrow self.clients as mutable). The code has been edited in vim and the build is failing. |
| 11 kernel module | nvme-stats-driver / loading nvme_stats driver / Loading the nvme_stats driver in ~/kmod/nvme-stats (branch main). Built the kernel module with make, but insmod failed with an error about an invalid module format. Dmesg shows the kernel version mismatch: expected '6.8.0-52-generic' but got '6.8.0-51-generic'. | nvme-stats-build / building nvme_stats kernel module / Attempting to build the nvme_stats kernel module from /lib/modules/6.8.0/build. Insmod failed with "Invalid module format" error, but dmesg output confirms the module version is incorrect (6.8.0-51-generic vs 6.8.0-52-generic). |
| 14b db migration | acme-db-tenant / adding tenant support to acme database / Adding tenant support to the acme database in ~/work/acme (branch feature/dashboard). Created a new migration version in migrations/versions/0042_tenant.py, edited it, and ran alembic upgrade head to apply the changes. However, alembic encountered an error because the tenant_id column already exists, indicating a conflict with the current schema. | acme-db-migration / database migration for tenant_id column in billing / Attempted to upgrade the billing database schema by running alembic upgrade head. The migration failed because the column "tenant_id" already exists in the current database state. |
| 15 rolling, step 1 | nt-proto-fix / fixing the reconnect protocol in nt / Working on the reconnect protocol in ~/src/nt (branch reconnect). Checked out the branch, built the project with cargo, opened proto.rs for editing, and started testing with cargo test. However, an error occurred with mismatched types at line 88 in proto.rs, preventing the successful compilation of the nt library. | nt-reconnect / reconnecting git branch in ~/src/nt / Reconnected git branch 'reconnect' in ~/src/nt. After building the project with cargo and editing the proto.rs file, the test suite failed with a type mismatch error. |
| 15 rolling, step 2 | nt-reconnect / implementing reconnect protocol in nt / Implementing the reconnect protocol in ~/src/nt (branch reconnect). Edited proto.rs in vim, ran cargo test which passed, and started building the nt library with make deploy. The build is currently at 45% completion. | nt-project / fixing type errors in nt project / Reconnected git branch 'reconnect' in ~/src/nt. After editing proto.rs and building with cargo, the test suite passed but the make deploy command failed to upload 45% of the code. |
| 16 rolling, step 2 | nas01-update / updating nas01 system / Updated the system on nas01 (~). The system has been up for 63 days. Apt was used to update packages, and the upgrade was successful. Rebooted the system, connected via ssh, and verified the kernel version and the status of the SMB service. | nas01-ssh-upgrade / upgrading linux-image-6.8.0-52-generic to 6.8.0-52.53 / SSH connection established to nas01. Uptime shows 63 days, 3 hours, and 12 minutes. Upgraded linux-image-6.8.0-52-generic to 6.8.0-52.53. (unchanged from step 1: the reboot is missing) |
| 17 rolling, step 2 | resnet-cifar-training / training ResNet-CIFAR model / Training the ResNet-CIFAR model in ~/ml/resnet-cifar (branch main). Edited configs/default.yaml to adjust settings. Training with python train.py for 10 epochs with a batch size of 16 failed with a torch.OutOfMemoryError after 7 epochs, with loss 0.314 and accuracy 0.887. (wrong: still running) | resnet-cifar-training / training resnet-cifar model on RTX 3060 / Attempting to train the resnet-cifar model on NVIDIA RTX 3060. CUDA out of memory error occurred when trying to allocate 2.40 GiB for the model during training. Training started with 10 epochs and 16 batches, achieving 88.7% accuracy after 7 epochs. |

Run: `python3 prompt_from_rs.py v3; python3 run.py --cache --models granite-4.0-1b-Q4_K_M.gguf Qwen3.5-0.8B-Q4_K_M.gguf --prompt v3 --threads 1 2 --out results/v3.json; python3 report_v3.py results/v3.json --bad`.

## Rerun
```
bench/run.sh                       # builds llama-server, downloads the models, runs everything (hours of wall time on a slow box)
python3 bench/report.py bench/results/full-v2.json
python3 bench/run.py --models granite-4.0-1b-Q4_K_M.gguf --threads 2 --cache --out /tmp/one.json   # a single model
```
Models and the llama.cpp build live in `~/.cache/nt-bench`. `run.py` resumes from its output file.
