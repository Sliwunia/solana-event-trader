#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ -f .tools/local-demo-bot.pid ]]; then
    trader_pid=$(cat .tools/local-demo-bot.pid)
    if [[ "$trader_pid" =~ ^[0-9]+$ && -r "/proc/$trader_pid/cmdline" ]]; then
        trader_command=$(tr '\0' ' ' < "/proc/$trader_pid/cmdline")
        trader_directory=$(readlink "/proc/$trader_pid/cwd")
        if [[ "$trader_directory" == "$PWD" && "$trader_command" == *solana-event-trader*config.local.toml* ]]; then
            kill -INT "$trader_pid"
            printf 'Zatrzymywanie lokalnego testu.\n'
            for _ in $(seq 1 40); do
                if ! kill -0 "$trader_pid" 2>/dev/null; then break; fi
                sleep 0.25
            done
            if [[ -f .tools/local-validator.pid ]]; then
                validator_pid=$(cat .tools/local-validator.pid)
                if [[ "$validator_pid" =~ ^[0-9]+$ ]]; then
                    for _ in $(seq 1 40); do
                        if ! kill -0 "$validator_pid" 2>/dev/null; then break; fi
                        sleep 0.25
                    done
                fi
            fi
            exit 0
        fi
    fi
fi
printf 'Lokalny bot nie jest uruchomiony.\n'
