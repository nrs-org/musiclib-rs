#!/usr/bin/env bash
set -euo pipefail

RUST_LOG=musiclib_rs=info,dedup=info cargo run --bin dedup -- "$@"
