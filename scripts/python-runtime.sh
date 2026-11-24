#!/bin/sh
# Fetch the pinned standalone CPython that hermes-python-host embeds and the packages ship
# (python-build-standalone, install_only_stripped), verify it, and print the environment a
# build needs, as KEY=VALUE lines: PYO3_PYTHON (the interpreter) and PYO3_CONFIG_FILE. The
# explicit PyO3 config is needed because these builds report a placeholder library folder
# (/install/lib) in sysconfig. CI appends the lines to $GITHUB_ENV; the packaging scripts
# export them.
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
python="$root/python/bin/python3"
pyver=$("$python" -c 'import sys; print("%d.%d" % sys.version_info[:2])')
if [ "$(uname -s)" = Darwin ]; then
  # Link and load it by @rpath, so the host finds it next to itself once packaged.
  library="$root/python/lib/libpython$pyver.dylib"
  if ! otool -D "$library" | grep -q '^@rpath/'; then
    install_name_tool -id "@rpath/libpython$pyver.dylib" "$library"
    codesign --force --sign - "$library"
  fi
fi
cat > "$root/pyo3-config.txt" <<CONFIG
implementation=CPython
version=$pyver
shared=true
abi3=false
lib_name=python$pyver
lib_dir=$root/python/lib
executable=$python
pointer_width=64
build_flags=
suppress_build_script_link_lines=false
CONFIG
printf 'PYO3_PYTHON=%s\nPYO3_CONFIG_FILE=%s\n' "$python" "$root/pyo3-config.txt"
