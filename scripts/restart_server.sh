#!/usr/bin/env bash
# Restart the local musiclib-rs server (src/bin/server): stop whatever's
# currently running from target/{debug,release}/server, rebuild, and run it
# in the foreground. Extra args are forwarded, e.g.:
#   ./scripts/restart_server.sh --listen 127.0.0.1:4600
set -euo pipefail

find_pids() {
    # `|| true` on each: pgrep exits 1 (not an error here) when a pattern
    # simply has no match, which `pipefail` would otherwise turn into a
    # failure of the whole substitution below.
    { pgrep -f 'target/debug/server' || true; pgrep -f 'target/release/server' || true; } 2>/dev/null | sort -u
}

pids="$(find_pids)"
if [[ -n "$pids" ]]; then
    echo "stopping existing server (pid: $(echo "$pids" | tr '\n' ' '))" >&2
    kill $pids
    for _ in $(seq 1 50); do
        [[ -z "$(find_pids)" ]] && break
        sleep 0.1
    done
    pids="$(find_pids)"
    if [[ -n "$pids" ]]; then
        echo "still running after SIGTERM, sending SIGKILL" >&2
        kill -9 $pids
    fi
fi

exec cargo run --bin server -- "$@"
