#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
trader_cache="${XDG_CACHE_HOME:-$HOME/.cache}/solana-event-trader"
export CARGO_HOME="$trader_cache/cargo" CARGO_TARGET_DIR="$trader_cache/target"
if [[ -x "$PWD/.tools/cargo/bin/cargo" && -d "$PWD/.tools/rustup" ]]; then
    export RUSTUP_HOME="$PWD/.tools/rustup"
    export PATH="$PWD/.tools/cargo/bin:$PATH"
fi
export SOLANA_RAYON_THREADS=2 RAYON_NUM_THREADS=2
mkdir -p .tools dist/linux-x86_64
demo_is_running=false
if [[ -f .tools/local-demo-bot.pid ]]; then
    previous_pid=$(cat .tools/local-demo-bot.pid)
    if [[ "$previous_pid" =~ ^[0-9]+$ && -r "/proc/$previous_pid/cmdline" ]]; then
        previous_command=$(tr '\0' ' ' < "/proc/$previous_pid/cmdline")
        previous_directory=$(readlink "/proc/$previous_pid/cwd")
        if [[ "$previous_directory" == "$PWD" && "$previous_command" == *solana-event-trader*config.local.toml* ]]; then
            demo_is_running=true
        fi
    fi
fi
if [[ "${1:-}" == '--status' ]]; then
    if "$demo_is_running"; then exit 0; else exit 1; fi
fi
if "$demo_is_running"; then
    printf 'Lokalny test juz dziala. Log: .tools/local-demo.log. Zatrzymanie: .\stop-local.ps1\n'
    exit 0
fi
bash scripts/bootstrap-local.sh
printf 'Budowanie lokalnego testu...\n'
if ! cargo build --release --locked --bin solana-event-trader --example local_demo > .tools/local-build.log 2>&1; then
    tail -n 60 .tools/local-build.log >&2; exit 1
fi
install -m 755 "$CARGO_TARGET_DIR/release/examples/local_demo" dist/linux-x86_64/local-demo
install -m 755 "$CARGO_TARGET_DIR/release/solana-event-trader" dist/linux-x86_64/solana-event-trader
dist/linux-x86_64/local-demo prepare
validator="$trader_cache/local-tools/solana-release/bin/solana-test-validator"
ledger="$trader_cache/localnet-ledger"
node_pid=''
bot_pid=''
trigger_pid=''
cleanup() {
    trap - EXIT INT TERM
    if [[ -n "$trigger_pid" ]]; then kill "$trigger_pid" 2>/dev/null || true; fi
    if [[ -n "$bot_pid" ]]; then kill -INT "$bot_pid" 2>/dev/null || true; wait "$bot_pid" 2>/dev/null || true; fi
    if [[ -n "$node_pid" ]]; then kill "$node_pid" 2>/dev/null || true; wait "$node_pid" 2>/dev/null || true; fi
}
trap cleanup EXIT INT TERM
"$validator" --ledger "$ledger" --bind-address 127.0.0.1 --rpc-port 18899 \
    --faucet-port 18901 --gossip-port 18902 --dynamic-port-range 19000-19030 \
    --limit-ledger-size 50000 --account-dir "$PWD/.tools/local-fixtures/accounts" \
    --bpf-program SwapsVeCiPHMUAtzQWZw7RjsKjgCjhwU55QGu4U1Szw "$trader_cache/local-tools/spl-token-swap-devnet.so" \
    > .tools/local-validator.log 2>&1 &
node_pid=$!
printf '%s\n' "$node_pid" > .tools/local-validator.pid
sleep 1
if ! kill -0 "$node_pid" 2>/dev/null; then
    tail -n 40 .tools/local-validator.log >&2
    exit 1
fi
dist/linux-x86_64/local-demo configure
printf '\nLOKALNY TEST SOLANY. Fikcyjne tokeny. Bot dziala w trybie symulacji.\n'
dist/linux-x86_64/solana-event-trader config.local.toml 2>&1 &
bot_pid=$!
printf '%s\n' "$bot_pid" > .tools/local-demo-bot.pid
(
    sleep 4
    dist/linux-x86_64/local-demo trigger
) &
trigger_pid=$!
wait "$bot_pid"
