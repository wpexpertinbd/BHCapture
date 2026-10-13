#!/bin/bash
# Cross-build for Windows from this Mac (decision 23). Homebrew's cargo shadows rustup's and has no
# Windows std, so rustup's binaries are used explicitly. Usage: scripts/xwin.sh check|build [args...]
set -euo pipefail
TC="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin"
export PATH="$TC/bin:$HOME/.cargo/bin:/opt/homebrew/opt/llvm/bin:/opt/homebrew/opt/lld/bin:$PATH"
export RUSTC="$TC/bin/rustc"
cmd="$1"; shift
exec "$TC/bin/cargo" xwin "$cmd" --target x86_64-pc-windows-msvc "$@"
