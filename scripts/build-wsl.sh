#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# Store compiler caches on WSL's Linux filesystem and the deliverable in the project.
trader_cache="${XDG_CACHE_HOME:-$HOME/.cache}/solana-event-trader"
export CARGO_HOME="$trader_cache/cargo"
export CARGO_TARGET_DIR="$trader_cache/target"
if [[ -x "$PWD/.tools/cargo/bin/cargo" && -d "$PWD/.tools/rustup" ]]; then
    export RUSTUP_HOME="$PWD/.tools/rustup"
    export PATH="$PWD/.tools/cargo/bin:$PATH"
fi
if ! command -v cargo >/dev/null 2>&1; then
    printf 'Brak Rust w WSL. Zainstaluj Rust przez rustup i ponow probe.\n' >&2
    exit 1
fi

mkdir -p .tools dist/linux-x86_64 "$CARGO_HOME/registry"
# Reuse already downloaded archives, if available, without extracting crates on NTFS.
if [[ -d .tools/cargo/registry/cache ]]; then
    cp -an .tools/cargo/registry/cache "$CARGO_HOME/registry/"
fi
printf 'Budowanie release. Pelny log: .tools/build-wsl.log\n'
if ! cargo build --release --locked > .tools/build-wsl.log 2>&1; then
    tail -n 70 .tools/build-wsl.log >&2
    exit 1
fi
install -m 755 "$CARGO_TARGET_DIR/release/solana-event-trader" dist/linux-x86_64/solana-event-trader
tail -n 4 .tools/build-wsl.log
printf 'Gotowy plik: dist/linux-x86_64/solana-event-trader (Linux/WSL).\n'
