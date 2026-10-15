#!/usr/bin/env bash
# Daily library update, meant for cron (see docs/plan-update.md).
#
# Each file <config_dir>/update_roots/NAME.txt lists root URLs (one per line,
# `#` comments allowed) imported with fetch options NAME.yaml from
# <config_dir>/fetch_options/. Every run refreshes the growing listings its
# roots reach; the last one then spends the rest of the day's YouTube budget
# on full refreshes of the oldest uploads snapshots.
#
# Environment:
#   QUOTA_BUDGET  YouTube units the update may use per day (default 8000),
#                 leaving the rest of the 10k for manual refreshes.
#   ROOTS_DIR     roots directory (default <config_dir>/update_roots).
#   IMPORT        import command (default: cargo run --release --bin import --).
set -euo pipefail

config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/musiclib-rs"
roots_dir="${ROOTS_DIR:-$config_dir/update_roots}"
budget="${QUOTA_BUDGET:-8000}"
read -r -a import <<<"${IMPORT:-cargo run --release --bin import --}"

shopt -s nullglob
roots=("$roots_dir"/*.txt)
if ((${#roots[@]} == 0)); then
  echo "no roots files in $roots_dir" >&2
  exit 1
fi

export RUST_LOG="${RUST_LOG:-musiclib_rs=info,import=info}"
status=0
for i in "${!roots[@]}"; do
  file="${roots[$i]}"
  name="$(basename "$file" .txt)"
  extra=()
  if ((i < ${#roots[@]} - 1)); then
    extra+=(--no-full-refresh-pass)
  fi
  echo "== $name" >&2
  # One failing roots file shouldn't stop the others.
  "${import[@]}" --url-file "$file" --fetch-options "$name.yaml" \
    --refresh --quota-budget "$budget" "${extra[@]}" "$@" || status=$?
done
exit "$status"
