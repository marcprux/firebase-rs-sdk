#!/usr/bin/env python3
"""Measure how much of each JavaScript SDK product has a counterpart in this crate.

The JS SDK publishes an API Extractor report per package under `common/api-review/*.api.md`,
which lists every public entity it exports. This script reads those reports and looks for a
matching identifier in the corresponding Rust crate: `getDoc` matches `get_doc`, `DocumentSnapshot`
matches a type of the same name.

The match is by name, so the numbers are an estimate, and a deliberately pessimistic one: an API
we ship under a more idiomatic Rust name (`download_url_request` for `getDownloadURL`) counts as
missing, while a name that happens to appear in an unrelated place counts as present. Treat a
number as "roughly this much of the product exists", not as a score.

    scripts/api_parity.py ~/src/firebase-js-sdk            # summary table
    scripts/api_parity.py ~/src/firebase-js-sdk --missing  # and what has no counterpart

Clone the reports with:

    git clone --depth 1 https://github.com/firebase/firebase-js-sdk
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys

# JS package -> the crate that ports it. `firestore-lite` is listed because it is the shape this
# SDK's Firestore actually targets: one-shot reads and writes, no local cache.
PAIRS = [
    ("firestore", "firebase-firestore"),
    ("firestore-lite", "firebase-firestore"),
    ("auth", "firebase-auth"),
    ("database", "firebase-database"),
    ("ai", "firebase-ai"),
    ("data-connect", "firebase-data-connect"),
    ("analytics", "firebase-analytics"),
    ("storage", "firebase-storage"),
    ("app", "firebase-core"),
    ("remote-config", "firebase-remote-config"),
    ("app-check", "firebase-app-check"),
    ("messaging", "firebase-messaging"),
    ("functions", "firebase-functions"),
    ("installations", "firebase-installations"),
    ("performance", "firebase-performance"),
]

ENTITY = re.compile(r"^export (?:declare )?(?:abstract )?(function|class|interface|type|const|enum) ([A-Za-z_]\w*)")


def snake(name: str) -> str:
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def entities(report: pathlib.Path) -> list[tuple[str, str]]:
    return [m.groups() for m in (ENTITY.match(line) for line in report.read_text().splitlines()) if m]


def crate_text(crate: pathlib.Path) -> str:
    return "\n".join(path.read_text(errors="ignore") for path in crate.rglob("*.rs"))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("js_sdk", type=pathlib.Path, help="path to a firebase-js-sdk checkout")
    parser.add_argument("--missing", action="store_true", help="list the entities with no counterpart")
    args = parser.parse_args()

    reports = args.js_sdk / "common" / "api-review"
    if not reports.is_dir():
        print(f"no API reports under {reports}", file=sys.stderr)
        return 1

    root = pathlib.Path(__file__).resolve().parent.parent
    print(f"{'js package':<18}{'matched':>9}{'total':>7}{'share':>8}   crate")
    gaps: list[tuple[str, list[str]]] = []
    for package, crate_name in PAIRS:
        report = reports / f"{package}.api.md"
        crate = root / "crates" / crate_name / "src"
        if not report.exists() or not crate.is_dir():
            continue
        text = crate_text(crate)
        missing = []
        matched = 0
        names = entities(report)
        for _kind, name in names:
            if re.search(rf"\b{re.escape(name)}\b", text) or re.search(rf"\b{re.escape(snake(name))}\b", text):
                matched += 1
            else:
                missing.append(name)
        print(f"{package:<18}{matched:>9}{len(names):>7}{matched / len(names):>7.0%}   {crate_name}")
        gaps.append((package, missing))

    if args.missing:
        for package, missing in gaps:
            if missing:
                print(f"\n--- {package}: {len(missing)} without a counterpart")
                print("    " + ", ".join(missing))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
