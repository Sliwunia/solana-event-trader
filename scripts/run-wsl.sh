#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ ! -x dist/linux-x86_64/solana-event-trader ]]; then
    bash scripts/build-wsl.sh
fi
exec dist/linux-x86_64/solana-event-trader "$@"
