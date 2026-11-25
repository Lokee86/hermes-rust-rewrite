#!/usr/bin/env bash
# Start the packaged app under the current X display and require its window.
set -euo pipefail
archive=$1; picture=$2
work=$(mktemp -d)
tar -C "$work" -xzf "$archive"
# No GPU here; the app's own software renderer.
HERMES_RUST_DATA_DIR="$work/data" ICED_BACKEND=tiny-skia "$work/Hermes/bin/hermes" > "$work/log" 2>&1 &
app=$!
trap 'kill $app 2>/dev/null || true' EXIT
for _ in $(seq 1 60); do
    if ! kill -0 $app 2>/dev/null; then cat "$work/log"; echo "Hermes exited" >&2; exit 1; fi
    window=$(xdotool search --onlyvisible --name Hermes 2>/dev/null | head -1 || true)
    if [ -n "$window" ]; then
        sleep 8
        kill -0 $app || { cat "$work/log"; exit 1; }
        import -window root "$picture"
        colors=$(identify -format '%k' "$picture")
        echo "window $window, $colors colors"
        if [ "$colors" -lt 20 ]; then echo "The window is blank" >&2; exit 1; fi
        exit 0
    fi
    sleep 1
done
cat "$work/log"; echo "No window appeared" >&2; exit 1
