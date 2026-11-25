#!/usr/bin/env python3
"""Audit upstream RustSec advisories for Git-sourced glib."""

import json
import subprocess
import tempfile
import tomllib
from pathlib import Path


BACKPORT_REVISION = "036f8720136dac1200992639b648b968623e6583"
BACKPORT_SOURCE = (
    "git+https://github.com/Lokee86/hermes-rust-rewrite.git"
    f"?rev={BACKPORT_REVISION}#{BACKPORT_REVISION}"
)


def main():
    lockfile = tomllib.loads(Path("Cargo.lock").read_text(encoding="utf-8"))
    for package in lockfile["package"]:
        source = package.get("source", "")
        if package["name"] != "glib" or not source.startswith("git+"):
            continue

        version = package["version"]
        with tempfile.TemporaryDirectory() as directory:
            audit_lock = Path(directory) / "Cargo.lock"
            # RustSec matches on source as well as name and version.
            audit_lock.write_text(
                'version = 3\n\n[[package]]\nname = "glib"\n'
                f"version = {json.dumps(version)}\n"
                'source = "registry+https://github.com/rust-lang/crates.io-index"\n',
                encoding="utf-8",
            )
            # The preceding workspace audit has already fetched the advisory database.
            command = ["cargo", "audit", "--no-fetch", "--file", str(audit_lock)]
            if version == "0.18.5" and source == BACKPORT_SOURCE:
                command += ["--ignore", "RUSTSEC-2024-0429"]
            result = subprocess.run(command, check=False)
            if result.returncode:
                raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
