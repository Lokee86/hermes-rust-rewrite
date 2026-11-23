"""Run the pinned stock and Rust shell classifiers without executing a command.

Run this with the Python environment that runs the pinned Hermes Agent.

  cargo build -p hermes-engine --example composer_classifier
  python scripts/compare_composer_classifiers.py --source-checkout ../hermes-agent \
      --rust-classifier target/debug/examples/composer_classifier

On Windows point --rust-classifier at composer_classifier.exe.
"""
from __future__ import annotations

import argparse
from collections import Counter
from contextlib import chdir, redirect_stdout
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
from unittest.mock import patch

sys.dont_write_bytecode = True
from verify_stock_composer_shell import PINNED_STOCK_SHA, fixture_env, stock_source


COMMON = {
    # These established approval cases use the isolated fixture and are never run.
    "refused control": (
        'rm -rf absent', 'chmod 777 absent', 'git reset --hard',
    ),
    "literal and operator": (
        'echo hello', 'echo "hello world"', 'echo "café"', 'echo café',
        'echo hello && echo world', 'echo hello || echo world',
        'echo hello > result.txt', 'echo hello >> result.txt',
        'echo hello 1>result.txt', 'echo hello 2>&1',
        'echo hello > "output folder/output file.txt"',
    ),
    "external program": (
        'git status --short', 'git log -1 --oneline', 'git diff --stat',
        'rg --version', 'cargo --version', 'node --version', 'npm --version',
        'go version', 'dotnet --version', 'python --version',
        'python -m pip --version', 'python probe.py',
        'python probe.py "two words"', 'python "folder name/probe.py"',
        'python probe.py && echo finished', 'git --version',
    ),
}
POSIX = {
    "builtin and composition": (
        'pwd', 'pwd -P', "printf '%s\\n' hello | wc -l",
        "printf '%s\\n' hi | grep -F hi",
        'cd "folder name" && pwd', 'echo one; echo two',
        'echo one\necho two', 'echo "one\ntwo"',
        'if test -d .; then pwd; fi', 'for item in one two; do echo "$item"; done',
        "cat <<'EOF'\nplain text\nEOF", 'echo "$(printf hello)"',
        'echo prefix-"two words"', 'echo hello\\ world',
        'echo ready # a plain comment', 'echo "$COMPOSER_FIXTURE_ENV"',
        'COMPOSER_FORMAT=plain echo hello', 'env COMPOSER_FORMAT=plain echo hello',
    ),
    "paths and options": (
        'ls -la .', 'ls *.txt', 'ls ../fixture', 'ls .config',
        'cat ./notes.txt', 'cat ../fixture/notes.txt', 'cat "folder name/file name.txt"',
        'cat "folder name/café.txt"', 'cat .gitignore', 'cat "notes+more.txt"',
        'cat < notes.txt', 'cat < ../fixture/notes.txt',
        'head -n 2 notes.txt', 'head -2 notes.txt', 'head --lines=2 notes.txt',
        "grep -F 'one two' notes.txt", "grep -e 'one two' notes.txt",
        "grep --color=never hi notes.txt", 'ls --color=never', 'sort -u notes.txt',
    ),
    "external program": (
        '/bin/ls -al', '/usr/bin/wc -l notes.txt',
        'command ls -a', 'which python', 'stat notes.txt',
        "find . -type f -name '*.txt'", "sed -n '1,2p' notes.txt",
        "awk '{ print $1 }' notes.txt", 'cut -d , -f 1 notes.txt',
        'uname -a', 'id -un', 'date -u', 'tee result.txt',
    ),
}
WINDOWS = {
    "builtin and composition": (
        'cd', 'dir', 'ver', 'echo ready & echo done', 'echo ready | findstr ready',
        'echo ready && dir /b', 'echo ready > nul', 'echo ready 2>nul',
        'echo %COMPOSER_FIXTURE_ENV%', 'echo "%COMPOSER_FIXTURE_ENV%"',
        'echo plain!text', 'echo "two words" && echo done', 'echo "a & b"',
        'echo "literal | text" | findstr literal', 'if exist notes.txt type notes.txt',
        'for %f in (notes.txt) do @echo %f', 'rem an ordinary comment',
    ),
    "paths and options": (
        'dir /b /a', 'dir /a:d', 'dir /o:n', 'dir *.txt', 'dir ..',
        'dir .config', 'type notes.txt', r'type .\notes.txt',
        r'type "..\fixture\notes.txt"', r'type "folder name\file name.txt"',
        'type "folder name/file name.txt"', r'type "folder name\café.txt"',
        'type .gitignore', 'type "notes+more.txt"', 'findstr /i one notes.txt',
        'findstr /c:"one two" notes.txt', 'findstr /s /i one notes.txt',
        'findstr /n one *.txt', 'findstr /l /c:"one two" "folder name/file name.txt"',
    ),
    "external program": (
        'where git', 'where python', 'whoami', 'hostname', 'ipconfig',
        'find "one" notes.txt', 'more notes.txt', 'fc notes.txt other.txt',
        'doskey /history', 'timeout /t 1', 'git.exe --version',
        'powershell -NoProfile -Command "Get-Date"',
    ),
}


def corpus(cwd, interpreter):
    native = WINDOWS if os.name == "nt" else POSIX
    quoted = (lambda text: f'"{text}"') if os.name == "nt" else shlex.quote
    absolute = {
        "resolved path": (
            f'{quoted(interpreter)} probe.py',
            f'python {quoted(str(cwd / "probe.py"))}',
            f'echo hello > {quoted(str(cwd / "result.txt"))}',
            ('type ' if os.name == "nt" else 'cat ') + quoted(str(cwd / "notes.txt")),
        ),
    }
    for group in (COMMON, native, absolute):
        for category, commands in group.items():
            yield from ((category, command) for command in commands)


def stock_results(commands):
    # Verify the genuine lazy modules import successfully so an ImportError
    # cannot make the reference accidentally permissive.
    from cron import lifecycle_guard  # noqa: F401
    from tools import approval_detection as stock

    results = []
    for command in commands:
        hardline, reason = stock.detect_hardline_command(command)
        if hardline:
            results.append((False, reason))
        else:
            dangerous, _, reason = stock.detect_dangerous_command(command)
            results.append((not dangerous, reason))
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-checkout", type=Path, required=True)
    parser.add_argument("--rust-classifier", type=Path, required=True)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="hermes-classifier-") as tempdir:
        root = Path(tempdir)
        source = stock_source(args.source_checkout.resolve(strict=True), root)
        cwd = root / "working dir"
        cwd.mkdir()
        (cwd / "probe.py").touch()
        (cwd / "folder name").mkdir()
        (cwd / "folder name" / "probe.py").touch()
        profile = root / "profile"
        profile.mkdir()
        environment = fixture_env(profile, root / "home")
        git = shutil.which("git")
        if git:
            environment["PATH"] += os.pathsep + str(Path(git).parent)
        cases = list(corpus(cwd, sys.executable))
        commands = [command for _, command in cases]

        sys.path.insert(0, str(source))
        with patch.dict(os.environ, environment, clear=True), chdir(cwd):
            with redirect_stdout(sys.stderr):
                stock = stock_results(commands)
        native = json.loads(subprocess.run(
            [args.rust_classifier.resolve(strict=True)],
            input=json.dumps({"cwd": str(cwd), "commands": commands}),
            env=environment,
            cwd=cwd,
            capture_output=True,
            text=True,
            check=True,
        ).stdout)
        if len(native) != len(cases) or any(type(verdict) is not bool for verdict in native):
            raise ValueError("native classifier returned an invalid verdict list")

        rows = [
            {"category": category,
             "command": command.replace(sys.executable, "<python>").replace(str(cwd), "<cwd>"),
             "stock_allows": stock_allows,
             "stock_reason": stock_reason, "native_allows": native_allows}
            for (category, command), (stock_allows, stock_reason), native_allows
            in zip(cases, stock, native, strict=True)
        ]
        verdicts = Counter((row["stock_allows"], row["native_allows"]) for row in rows)
        counts = {
            "total": len(rows),
            "both_allow": verdicts[True, True],
            "both_refuse": verdicts[False, False],
            "stock_only": verdicts[True, False],
            "rust_only": verdicts[False, True],
        }
        if args.json:
            print(json.dumps({
                "stock_revision": PINNED_STOCK_SHA,
                "platform": sys.platform,
                "counts": counts,
                "cases": rows,
            }, indent=2))
        else:
            print(f"Stock {PINNED_STOCK_SHA} ({sys.platform}): {counts['total']} commands")
            print(f"Both allow {counts['both_allow']}. Both refuse {counts['both_refuse']}. "
                  f"Stock only {counts['stock_only']}. Rust only {counts['rust_only']}.")
            for row in rows:
                if row["stock_allows"] != row["native_allows"]:
                    owner = "stock" if row["stock_allows"] else "Rust"
                    print(f"{owner:5} {row['category']:24} {row['command']!a}")
        return int(bool(counts["stock_only"] or counts["rust_only"]))


if __name__ == "__main__":
    raise SystemExit(main())
