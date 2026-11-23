"""Export regex rules from the pinned, locally available stock Hermes commit."""
import argparse
import importlib.abc
import importlib.util
import json
from pathlib import Path
import re
from re import _constants as sre
from re import _parser as sre_parser
import subprocess
import sys

REVISION = "caafe5389ae585ef77387a962aee05b1bcf17029"


def required_literals(items):
    """Pick a literal or alternatives that must occur in a regex match."""
    choices = []
    literal = ""

    def flush():
        nonlocal literal
        if literal and literal.isascii():
            choices.append((literal.lower(),))
        literal = ""

    for operation, argument in items:
        if operation is sre.LITERAL:
            literal += chr(argument)
            continue
        flush()
        if operation is sre.SUBPATTERN:
            choices.append(required_literals(argument[-1]))
        elif operation is sre.ATOMIC_GROUP:
            choices.append(required_literals(argument))
        elif operation in (sre.MAX_REPEAT, sre.MIN_REPEAT, sre.POSSESSIVE_REPEAT):
            if argument[0] > 0:
                choices.append(required_literals(argument[-1]))
        elif operation is sre.BRANCH:
            alternatives = [required_literals(branch) for branch in argument[1]]
            if all(alternatives):
                choices.append(tuple(sorted({text for branch in alternatives for text in branch})))
    flush()
    return max(filter(None, choices), key=lambda items: min(map(len, items)), default=())


def generate(repo):
    def git(*command):
        return subprocess.check_output(
            ["git", "-C", repo, *command], text=True, encoding="utf-8"
        )

    files = set(git("ls-tree", "-r", "--name-only", REVISION).splitlines())

    class PinnedModules(importlib.abc.MetaPathFinder, importlib.abc.Loader):
        def find_spec(self, fullname, path=None, target=None):
            stem = fullname.replace(".", "/")
            for name, package in [(stem + "/__init__.py", True), (stem + ".py", False)]:
                if name in files:
                    return importlib.util.spec_from_loader(
                        fullname, self, origin=name, is_package=package
                    )

        def create_module(self, spec):
            return None

        def exec_module(self, module):
            name = module.__spec__.origin
            module.__file__ = str(Path(repo) / name)
            source = git("show", f"{REVISION}:{name}")
            exec(compile(source, name, "exec"), module.__dict__)

    finder = PinnedModules()
    sys.meta_path.insert(0, finder)
    try:
        from tools import approval_detection as stock
    finally:
        sys.meta_path.remove(finder)

    def regex(pattern, masked):
        literals = required_literals(sre_parser.parse(pattern.pattern, pattern.flags))
        return [pattern.pattern, int(pattern.flags), masked, literals]

    return {
        "stock_revision": REVISION,
        "interpreter_options": [
            [family, name.pattern, sorted(stock._INTERPRETER_EXEC_FLAGS[family]),
             sorted(stock._INTERPRETER_WITH_ARG[family])]
            for family, name in stock._INTERPRETER_NAME_RES
        ],
        "read_tool_options": {
            name: [sorted(flags), sorted(stock._READ_TOOL_LONG_OPTIONS_WITH_ARG[name]),
                   "".join(sorted(stock._READ_TOOL_SHORT_OPTIONS_WITH_ARG[name]))]
            for name, flags in sorted(stock._READ_TOOL_EXEC_FLAGS.items())
        },
        "hardline": [
            regex(pattern, masked)
            for pattern, _, masked in stock.HARDLINE_PATTERNS_COMPILED
        ],
        "dangerous": [
            regex(pattern, name in stock._QUOTE_MASKED_DANGEROUS_DESCRIPTIONS
                  or name == stock._PS_PIPE_ALIAS_DESCRIPTION)
            for pattern, name in stock.DANGEROUS_PATTERNS_COMPILED
            if isinstance(pattern, re.Pattern)
        ],
        "custom_matchers": [
            type(pattern).__name__
            for pattern, _ in stock.DANGEROUS_PATTERNS_COMPILED
            if not isinstance(pattern, re.Pattern)
        ],
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("stock_repo")
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    data = generate(args.stock_repo)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(data, ensure_ascii=True, indent=2) + "\n", encoding="utf-8", newline="\n")
