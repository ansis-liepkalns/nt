#!/usr/bin/env bash
# Reproduce the benchmark (about 6 GB of downloads, an hour of CPU). Needs cmake, a C++ compiler, git, curl, python3.
# Everything heavy lives in ~/.cache/nt-bench; results go to bench/results/ (git-ignored).
set -euo pipefail
cd "$(dirname "$0")"
C=~/.cache/nt-bench; mkdir -p $C/models results
if [ ! -x $C/build/bin/llama-server ]; then
  [ -d $C/src ] || git clone --depth 1 https://github.com/ggml-org/llama.cpp $C/src
  cmake -S $C/src -B $C/build -DCMAKE_BUILD_TYPE=Release -DGGML_NATIVE=ON -DLLAMA_CURL=OFF
  cmake --build $C/build -j"$(nproc)" --target llama-server
fi
dl() { [ -s $C/models/$2 ] || curl -fsSL -C - -o $C/models/$2 "https://huggingface.co/$1/resolve/main/$2"; }
dl unsloth/SmolLM2-135M-Instruct-GGUF SmolLM2-135M-Instruct-Q4_K_M.gguf
dl unsloth/SmolLM2-135M-Instruct-GGUF SmolLM2-135M-Instruct-Q8_0.gguf
dl bartowski/SmolLM2-360M-Instruct-GGUF SmolLM2-360M-Instruct-Q4_K_M.gguf
dl bartowski/SmolLM2-360M-Instruct-GGUF SmolLM2-360M-Instruct-Q8_0.gguf
dl unsloth/Qwen3-0.6B-GGUF Qwen3-0.6B-Q4_K_M.gguf
dl unsloth/Qwen3-1.7B-GGUF Qwen3-1.7B-Q4_K_M.gguf
dl unsloth/Qwen3.5-0.8B-GGUF Qwen3.5-0.8B-Q4_K_M.gguf
dl unsloth/gemma-3-270m-it-GGUF gemma-3-270m-it-Q4_K_M.gguf
dl unsloth/gemma-3-270m-it-GGUF gemma-3-270m-it-Q8_0.gguf
dl unsloth/gemma-3-1b-it-GGUF gemma-3-1b-it-Q4_K_M.gguf
dl LiquidAI/LFM2.5-230M-GGUF LFM2.5-230M-Q4_K_M.gguf
dl LiquidAI/LFM2.5-230M-GGUF LFM2.5-230M-Q8_0.gguf
dl LiquidAI/LFM2.5-350M-GGUF LFM2.5-350M-Q4_K_M.gguf
dl LiquidAI/LFM2.5-350M-GGUF LFM2.5-350M-Q8_0.gguf
dl LiquidAI/LFM2.5-1.2B-Instruct-GGUF LFM2.5-1.2B-Instruct-Q4_K_M.gguf
dl ibm-granite/granite-4.0-h-350m-GGUF granite-4.0-h-350m-Q4_K_M.gguf
dl ibm-granite/granite-4.0-h-350m-GGUF granite-4.0-h-350m-Q8_0.gguf
dl ibm-granite/granite-4.0-1b-GGUF granite-4.0-1b-Q4_K_M.gguf
dl unsloth/Llama-3.2-1B-Instruct-GGUF Llama-3.2-1B-Instruct-Q4_K_M.gguf
# full run with the winning prompt, 1 and 2 threads
python3 run.py --models $(cd $C/models && ls *.gguf) --prompt v2 --threads 1 2 --out results/full-v2.json
# same, but reusing the KV cache of the constant system+few-shot prefix (what the daemon would do)
python3 run.py --cache --models granite-4.0-1b-Q4_K_M.gguf Qwen3-1.7B-Q4_K_M.gguf Qwen3.5-0.8B-Q4_K_M.gguf gemma-3-1b-it-Q4_K_M.gguf LFM2.5-1.2B-Instruct-Q4_K_M.gguf granite-4.0-h-350m-Q8_0.gguf SmolLM2-135M-Instruct-Q8_0.gguf Qwen3-0.6B-Q4_K_M.gguf --prompt v2 --threads 1 2 --out results/cached-v2.json
python3 report.py results/full-v2.json > results/tables.md
