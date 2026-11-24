#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
destination=${1:-"$PWD/dist/Hermes.app"}
# hermes-python-host embeds this exact CPython; it ships in Contents/Resources/python.
if [ -z "${PYO3_CONFIG_FILE:-}" ]; then
    for setting in $(scripts/python-runtime.sh); do export "$setting"; done
fi
cargo build --locked --release --features embedded-herdr -p hermes-desktop
cargo build --locked --release -p hermes-python-host
target_dir=$(cargo metadata --no-deps --format-version 1 | plutil -extract target_directory raw -o - -)
mkdir -p "$destination/Contents/MacOS" "$destination/Contents/Resources"
install -m 755 "$target_dir/release/hermes-desktop" "$destination/Contents/MacOS/hermes-desktop"
if [ -f "$destination/Contents/MacOS/hermes-gateway" ]; then
    rm "$destination/Contents/MacOS/hermes-gateway"
fi
install -m 755 "$target_dir/release/hermes-python-host" "$destination/Contents/MacOS/hermes-python-host"
scripts/bundle-python.sh "$PYO3_PYTHON" "$destination/Contents/Resources/python"
host="$destination/Contents/MacOS/hermes-python-host"
library=$(otool -L "$host" | awk '/libpython3|Python\.framework/ { print $1; exit }')
pyver=$("$PYO3_PYTHON" -c 'import sys; print("%d.%d" % sys.version_info[:2])')
case "$library" in
    @rpath/*) ;;
    *) install_name_tool -change "$library" "@rpath/libpython$pyver.dylib" "$host" ;;
esac
install_name_tool -id "@rpath/libpython$pyver.dylib" "$destination/Contents/Resources/python/lib/libpython$pyver.dylib"
find "$destination/Contents/Resources/python" -type f \( -name '*.dylib' -o -name '*.so' \) -exec codesign --force --sign - {} +
codesign --force --sign - "$host"
cp packaging/macos/Info.plist "$destination/Contents/Info.plist"
cp assets/icon.icns "$destination/Contents/Resources/Hermes.icns"
mkdir -p "$destination/Contents/Resources/Notices"
"$destination/Contents/MacOS/hermes-desktop" --licenses > "$destination/Contents/Resources/Notices/Herdr-THIRD-PARTY-NOTICES"
for notice in crates/desktop/assets/fonts/*NOTICE.txt crates/desktop/assets/fonts/*LICENSE*.txt; do
    if [ -f "$notice" ]; then cp "$notice" "$destination/Contents/Resources/Notices/"; fi
done
cp LICENSE-HERMES-AGENT "$destination/Contents/Resources/Notices/Hermes-LICENSE"
codesign --force --sign - "$destination"
scripts/verify-native-macos.sh "$destination"
printf 'Built %s\n' "$destination"
