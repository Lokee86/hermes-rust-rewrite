#!/bin/sh
# Fetch the pinned standalone CPython that hermes-python-host embeds and the packages ship
# (python-build-standalone, install_only_stripped), verify it, and print its interpreter
# path. Build with PYO3_PYTHON set to that path so the host links this exact runtime:
#   export PYO3_PYTHON=$(scripts/python-runtime.sh)
set -eu
cd "$(dirname "$0")/.."
release=20261026
version=3.13.18
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) triple=aarch64-apple-darwin; sha=8392b6ea51d94391befb2c31ebe75463f045d0b69dfcf5e1bb3234e1f5fa3405 ;;
  Linux-x86_64) triple=x86_64-unknown-linux-gnu; sha=df15fe60d3ff55bf9dcf25824b5572756bb8816ff93865a273d3301d7066a8b7 ;;
  *) echo "No pinned Python runtime for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac
root="$PWD/target/python-runtime/$version-$triple"
if [ ! -x "$root/python/bin/python3" ]; then
  archive="cpython-$version+$release-$triple-install_only_stripped.tar.gz"
  mkdir -p "$root"
  curl -fsSL --retry 3 -o "$root/$archive" "https://github.com/astral-sh/python-build-standalone/releases/download/$release/$archive"
  actual=$( (sha256sum "$root/$archive" 2>/dev/null || shasum -a 256 "$root/$archive") | cut -d' ' -f1)
  [ "$actual" = "$sha" ] || { echo "Checksum mismatch for $archive" >&2; exit 1; }
  tar -xzf "$root/$archive" -C "$root"
fi
printf '%s\n' "$root/python/bin/python3"
