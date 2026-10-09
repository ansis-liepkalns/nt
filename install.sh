#!/bin/sh
# Build nt and install it locally and/or on remote hosts.
#   ./install.sh [--slim] [--prefix DIR] [--remote [user@]host ...]
set -eu

prefix="$HOME/.local"
features=""
remotes=""

usage() {
    echo "usage: $0 [--slim] [--prefix DIR] [--remote [user@]host ...]" >&2
    exit "${1:-2}"
}

while [ $# -gt 0 ]; do
    case $1 in
        --slim|--no-llm) features="--no-default-features" ;;
        --prefix)
            [ $# -ge 2 ] || usage
            prefix=$2
            shift
            ;;
        --remote)
            [ $# -ge 2 ] || usage
            remotes="$remotes
$2"
            shift
            ;;
        -h | --help) usage 0 ;;
        *) usage ;;
    esac
    shift
done

cd "$(dirname "$0")"
command -v cargo >/dev/null 2>&1 || { echo "install.sh: cargo not found (install a Rust toolchain)" >&2; exit 1; }

echo "Building nt (release)..."
# shellcheck disable=SC2086
# Rust 1.87+ is needed (edition 2024 and dependencies); distro packages are often older.
if ! command -v cargo >/dev/null 2>&1; then
    echo "install.sh: cargo not found. Install Rust with rustup: https://rustup.rs" >&2
    exit 1
fi
rv=$(rustc --version | cut -d' ' -f2)
if [ "$(printf '%s\n1.87.0\n' "$rv" | sort -V | head -n1)" != "1.87.0" ]; then
    echo "install.sh: Rust $rv is too old, nt needs 1.87 or newer." >&2
    echo "  rustup users: rustup update stable" >&2
    echo "  distro Rust (apt): install rustup instead: https://rustup.rs" >&2
    exit 1
fi

# bindgen (llama.cpp bindings) needs clang's builtin headers such as stdbool.h. Distros often ship
# libclang without them (e.g. Debian/Ubuntu libclang1 without libclang-common-*-dev); borrow gcc's then.
if [ -z "$features" ] && [ -z "${BINDGEN_EXTRA_CLANG_ARGS:-}" ] && ! command -v clang >/dev/null 2>&1; then
    if command -v gcc >/dev/null 2>&1; then
        gcc_inc=$(gcc -print-file-name=include)
        if [ -f "$gcc_inc/stdbool.h" ]; then
            BINDGEN_EXTRA_CLANG_ARGS="-I$gcc_inc"
            export BINDGEN_EXTRA_CLANG_ARGS
            echo "install.sh: clang not found, using gcc headers for bindgen ($gcc_inc)"
        fi
    fi
fi

cargo build --release $features

bin=target/release/nt
dest="$prefix/bin/nt"
mkdir -p "$prefix/bin"
cp "$bin" "$dest.tmp"
chmod 755 "$dest.tmp"
mv "$dest.tmp" "$dest"
echo "Installed $dest"
case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) echo "warning: $prefix/bin is not on your PATH; add this to your shell rc file (e.g. ~/.bashrc):" >&2
       echo "    export PATH=\"$prefix/bin:\$PATH\"" >&2 ;;
esac

# Remote installs: one ssh per host; the binary is streamed and moved into place atomically.
IFS='
'
for host in $remotes; do
    [ -n "$host" ] || continue
    echo "Installing on $host..."
    if ! remote_id=$(ssh "$host" 'uname -s; uname -m'); then
        echo "install.sh: cannot reach $host over ssh" >&2
        exit 1
    fi
    local_id=$(uname -s; uname -m)
    if [ "$remote_id" != "$local_id" ]; then
        echo "install.sh: $host is $(echo "$remote_id" | tr '\n' ' ')but this machine is $(echo "$local_id" | tr '\n' ' '): build nt on the remote host instead" >&2
        exit 1
    fi
    ssh "$host" 'mkdir -p ~/.local/bin && cat > ~/.local/bin/nt.tmp && chmod +x ~/.local/bin/nt.tmp && mv ~/.local/bin/nt.tmp ~/.local/bin/nt' <"$bin"
    echo "Installed ~/.local/bin/nt on $host"
done
