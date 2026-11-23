#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
destination=${1:-"$PWD/dist/Hermes.app"}
cargo build --locked --release --features embedded-herdr -p hermes-desktop
target_dir=$(cargo metadata --no-deps --format-version 1 | plutil -extract target_directory raw -o - -)
mkdir -p "$destination/Contents/MacOS" "$destination/Contents/Resources"
install -m 755 "$target_dir/release/hermes-desktop" "$destination/Contents/MacOS/hermes-desktop"
if [ -f "$destination/Contents/MacOS/hermes-gateway" ]; then
    rm "$destination/Contents/MacOS/hermes-gateway"
fi
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
