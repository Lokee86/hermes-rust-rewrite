#!/bin/sh
# Build a relocatable Linux native distribution. Its bin and share directories
# also follow the XDG layout when installed under the same prefix.
set -eu
cd "$(dirname "$0")/.."
destination=${1:-"$PWD/dist"}
mkdir -p "$destination"
destination=$(cd "$destination" && pwd)
build=$(mktemp -d)
trap 'rm -rf "$build"' EXIT
# hermes-python-host embeds this exact CPython; it ships in lib/hermes/python.
if [ -z "${PYO3_CONFIG_FILE:-}" ]; then
    for setting in $(scripts/python-runtime.sh); do export "$setting"; done
fi
cargo build --locked --release -p hermes-desktop -p hermes-gateway -p hermes-python-host
target=${CARGO_TARGET_DIR:-"$PWD/target"}
stage="$build/Hermes"
install -d "$stage/bin" "$stage/share/applications" "$stage/share/icons/hicolor/512x512/apps" "$stage/share/licenses/hermes"
install -m755 "$target/release/hermes-desktop" "$target/release/hermes-gateway" "$target/release/hermes-python-host" "$stage/bin/"
scripts/bundle-python.sh "$PYO3_PYTHON" "$stage/lib/hermes/python"
"$stage/bin/hermes-python-host" --version | grep -q " $stage/lib/hermes/python\$" || { echo "hermes-python-host does not run the bundled Python" >&2; exit 1; }
install -m644 assets/icon.png "$stage/share/icons/hicolor/512x512/apps/com.nousresearch.hermes.rust-development.png"
install -m644 packaging/linux/com.nousresearch.hermes.rust-development.desktop "$stage/share/applications/"
install -m644 LICENSE-HERMES-AGENT "$stage/share/licenses/hermes/LICENSE"
for notice in crates/desktop/assets/fonts/*NOTICE.txt crates/desktop/assets/fonts/*LICENSE*.txt; do
  if [ -f "$notice" ]; then install -m644 "$notice" "$stage/share/licenses/hermes/"; fi
done
tar -C "$build" -czf "$destination/Hermes-linux-$(uname -m).tar.gz" Hermes
