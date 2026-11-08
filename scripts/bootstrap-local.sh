#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
trader_cache="${XDG_CACHE_HOME:-$HOME/.cache}/solana-event-trader"
local_tools="$trader_cache/local-tools"
mkdir -p "$local_tools"
archive="$local_tools/agave-v4.4.1.tar.bz2"
digest=5b0f0fc7e4eedace05f6455de00aa1f2f3d1642068f1e8d50760ba49112c3941
if [[ ! -x "$local_tools/solana-release/bin/solana-test-validator" ]]; then
    printf 'Pobieranie oficjalnego lokalnego walidatora Agave 4.4.1...\n'
    if [[ ! -f "$archive" ]]; then
        curl --proto '=https' --tlsv1.2 --fail --location --retry 2 \
          https://github.com/anza-xyz/agave/releases/download/v4.4.1/solana-release-x86_64-unknown-linux-gnu.tar.bz2 \
          -o "$archive"
    fi
    printf '%s  %s\n' "$digest" "$archive" | sha256sum --check --status
    tar -xjf "$archive" -C "$local_tools"
fi
"$local_tools/solana-release/bin/solana-test-validator" --version
if [[ ! -s "$local_tools/spl-token-swap-devnet.so" ]]; then
    "$local_tools/solana-release/bin/solana" program dump --url https://api.devnet.solana.com \
      SwapsVeCiPHMUAtzQWZw7RjsKjgCjhwU55QGu4U1Szw "$local_tools/spl-token-swap-devnet.so"
fi
sha256sum "$local_tools/spl-token-swap-devnet.so"
