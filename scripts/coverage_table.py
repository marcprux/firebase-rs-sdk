#!/usr/bin/env python3
"""Renders the coverage table in README.md from docs/coverage.toml.

The table used to be maintained by hand in three places at once — README.md, each module's
PORTING_STATUS.md and each module's README.md — and they disagreed by as much as fifty points.
docs/coverage.toml is now the only place the numbers live; run this script after editing it, and
CI runs `--check` so the two cannot drift apart again.

    scripts/coverage_table.py            # rewrite the table in README.md
    scripts/coverage_table.py --check    # fail if README.md is out of date
"""

import argparse
import pathlib
import sys

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11
    import tomli as tomllib  # type: ignore

ROOT = pathlib.Path(__file__).resolve().parent.parent
COVERAGE = ROOT / "docs" / "coverage.toml"
README = ROOT / "README.md"
BEGIN = "<!-- coverage:begin (generated from docs/coverage.toml by scripts/coverage_table.py) -->"
END = "<!-- coverage:end -->"


def render() -> str:
    data = tomllib.loads(COVERAGE.read_text())
    lines = [
        BEGIN,
        "",
        "| Module | Coverage | Backend | Verified | Notes |",
        "|---|---:|---|:---:|---|",
    ]
    for module, entry in data.items():
        lines.append(
            "| {module} | {coverage} | {backend} | {verified} | {notes} |".format(
                module=module,
                coverage=entry["coverage"],
                backend=entry["backend"],
                verified=entry["verified"],
                notes=entry["notes"],
            )
        )
    lines += ["", END]
    return "\n".join(lines)


def replace_block(text: str, block: str) -> str:
    start = text.index(BEGIN)
    end = text.index(END) + len(END)
    return text[:start] + block + text[end:]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="fail instead of rewriting")
    args = parser.parse_args()

    readme = README.read_text()
    if BEGIN not in readme or END not in readme:
        print(f"{README}: coverage markers are missing", file=sys.stderr)
        return 1

    updated = replace_block(readme, render())
    if updated == readme:
        return 0
    if args.check:
        print(
            "README.md is out of date with docs/coverage.toml; run scripts/coverage_table.py",
            file=sys.stderr,
        )
        return 1
    README.write_text(updated)
    print("README.md coverage table updated")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
