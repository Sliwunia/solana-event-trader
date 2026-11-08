#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
trader_cache="${XDG_CACHE_HOME:-$HOME/.cache}/solana-event-trader"
export CARGO_HOME="$trader_cache/cargo"
export CARGO_TARGET_DIR="$trader_cache/target"
if [[ -x "$PWD/.tools/cargo/bin/cargo" && -d "$PWD/.tools/rustup" ]]; then
    export RUSTUP_HOME="$PWD/.tools/rustup"
    export PATH="$PWD/.tools/cargo/bin:$PATH"
fi
mkdir -p .tools dist/linux-x86_64
printf 'Budowanie konfiguratora devnet. Log: .tools/setup-build.log\n'
if ! cargo build --release --locked --example setup_devnet > .tools/setup-build.log 2>&1; then
    tail -n 70 .tools/setup-build.log >&2
    exit 1
fi
install -m 755 "$CARGO_TARGET_DIR/release/examples/setup_devnet" dist/linux-x86_64/setup-devnet
exec dist/linux-x86_64/setup-devnet "$@"
