#!/bin/sh
# Start the packaged app and require a window of its own.
set -eu
app=$1; picture=$2
work=$(mktemp -d)
HERMES_RUST_DATA_DIR="$work/data" "$app/Contents/MacOS/hermes-desktop" > "$work/log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null || true' EXIT
if ! swift "$(dirname "$0")/smoke-macos.swift" "$pid"; then cat "$work/log"; exit 1; fi
kill -0 $pid
# Runners may not allow screen capture; the window list is the check.
screencapture -x "$picture" 2>/dev/null || true
