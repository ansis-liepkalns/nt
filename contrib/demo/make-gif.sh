#!/bin/sh
# Render the recorded cast to docs/demo.gif (needs agg: cargo install --git https://github.com/asciinema/agg).
set -e
cd "$(dirname "$0")/../.."
agg --font-size 16 --theme dracula --fps-cap 12 --idle-time-limit 2 --speed 1.35 contrib/demo/demo.cast docs/demo.gif
