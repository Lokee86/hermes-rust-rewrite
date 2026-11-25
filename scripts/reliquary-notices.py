#!/usr/bin/env python3
"""Stage the license texts for embedded Reliquary and verify the copies in release archives."""

import argparse
import json
import shutil
import subprocess
import tarfile
import zipfile
from pathlib import Path, PurePosixPath


def notices():
    host = subprocess.check_output(["rustc", "--print", "host-tuple"], text=True).strip()
    metadata = subprocess.check_output(
        ["cargo", "metadata", "--locked", "--filter-platform", host, "--format-version", "1"]
    )
    packages = json.loads(metadata)["packages"]

    def package(name):
        matches = [item for item in packages if item["name"] == name]
        if len(matches) != 1:
            raise SystemExit(f"expected one locked {name} package, found {len(matches)}")
        return matches[0]

    core = package("reliquary-memory")
    revision = core["source"].rsplit("#", 1)[1]
    notice = Path("RELIQUARY-NOTICE")
    source_url = f"https://github.com/Lokee86/Reliquary/archive/{revision}.tar.gz"
    if source_url not in notice.read_text(encoding="utf-8"):
        raise SystemExit(f"{notice} must offer the locked source: {source_url}")

    def root(name):
        return Path(package(name)["manifest_path"]).parent

    sources = {
        "Hermes-Rust-LICENSE": Path("LICENSE"),
        "Hermes-LICENSE": Path("LICENSE-HERMES-AGENT"),
        "Reliquary-NOTICE": notice,
        "Reliquary-MPL-2.0": root("reliquary-memory") / "LICENSE",
        "Lodestone-MIT": root("lodestone-core").parents[1] / "LICENSE.md",
    }
    if "linux" in host.split("-"):
        glib = root("glib")
        sources["glib-MIT"] = glib / "LICENSE"
        sources["glib-COPYRIGHT"] = glib / "COPYRIGHT"
    return source_url, sources


def verify(archive, sources):
    found = set()

    def check(name, read):
        # Windows ZipFile can emit backslashes. macOS also carries AppleDouble files.
        filename = PurePosixPath(name.replace("\\", "/")).name
        if filename in sources:
            if filename in found or read() != sources[filename].read_bytes():
                raise SystemExit(f"{archive}: duplicate or incorrect {filename}")
            found.add(filename)

    if archive.name.endswith(".tar.gz"):
        with tarfile.open(archive, "r|gz") as bundle:
            for member in bundle:
                if member.isfile():
                    check(member.name, lambda: bundle.extractfile(member).read())
    else:
        with zipfile.ZipFile(archive) as bundle:
            for member in bundle.infolist():
                if not member.is_dir():
                    check(member.filename, lambda: bundle.read(member))
    if missing := sources.keys() - found:
        raise SystemExit(f"{archive}: missing {', '.join(sorted(missing))}")
    print(f"Verified {len(found)} notices in {archive}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    stage = commands.add_parser("stage")
    stage.add_argument("destination", type=Path)
    stage.add_argument("executable", type=Path)
    stage.add_argument("--save-runtime-notices", type=Path)
    commands.add_parser("verify").add_argument("archive", nargs="+", type=Path)
    args = parser.parse_args()
    source_url, sources = notices()
    if args.command == "stage":
        # Piping a Windows GUI executable through PowerShell can discard its stdout.
        output = subprocess.check_output([args.executable.resolve(), "--licenses"], timeout=60)
        if source_url.encode() not in output:
            raise SystemExit(f"{args.executable} does not report the locked Reliquary source")
        args.destination.mkdir(parents=True, exist_ok=True)
        if args.save_runtime_notices:
            args.save_runtime_notices.write_bytes(output)
        for name, path in sources.items():
            shutil.copyfile(path, args.destination / name)
    else:
        for archive in args.archive:
            verify(archive, sources)


if __name__ == "__main__":
    main()
