#!/usr/bin/env bash
set -euo pipefail

RUST_LOG=musiclib_rs=info,import=info cargo run --bin import -- \
  'https://www.youtube.com/channel/UCqm3BQLlJfvkTsX_hvm0UmA' \
  --fetch-options config/fetch_options/vtuber_fetch_discography.yaml \
  "$@"
