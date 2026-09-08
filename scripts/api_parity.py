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

    scripts/api_parity.py packages/firebase-js-sdk            # summary table
    scripts/api_parity.py packages/firebase-js-sdk --missing   # and what has no counterpart
    scripts/api_parity.py packages/firebase-js-sdk --format markdown >> "$GITHUB_STEP_SUMMARY"
    scripts/api_parity.py packages/firebase-js-sdk --format json   # for tracking over time

`packages/` is gitignored so a reference checkout can live there:

    git clone --depth 1 https://github.com/firebase/firebase-js-sdk packages/firebase-js-sdk
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import subprocess
import sys

# JS package -> the crate that ports it, and whether it counts towards the overall total.
#
# `firestore-lite` is a subset of `firestore`, reported because it is the shape this SDK's
# Firestore actually targets — one-shot reads and writes, no local cache — but left out of the
# total so Firestore's entities are not counted twice.
#
# The order is fixed: the tables this produces are read across runs, and a row that moves for no
# reason but a changed percentage makes a diff harder to read.
PAIRS = [
    ("firestore", "firebase-firestore", True),
    ("firestore-lite", "firebase-firestore", False),
    ("auth", "firebase-auth", True),
    ("database", "firebase-database", True),
    ("ai", "firebase-ai", True),
    ("data-connect", "firebase-data-connect", True),
    ("analytics", "firebase-analytics", True),
    ("storage", "firebase-storage", True),
    ("app", "firebase-core", True),
    ("remote-config", "firebase-remote-config", True),
    ("app-check", "firebase-app-check", True),
    ("messaging", "firebase-messaging", True),
    ("functions", "firebase-functions", True),
    ("installations", "firebase-installations", True),
    ("performance", "firebase-performance", True),
]

ENTITY = re.compile(r"^export (?:declare )?(?:abstract )?(function|class|interface|type|const|enum) ([A-Za-z_]\w*)")


def snake(name: str) -> str:
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def entities(report: pathlib.Path) -> list[tuple[str, str]]:
    return [m.groups() for m in (ENTITY.match(line) for line in report.read_text().splitlines()) if m]


def crate_text(crate: pathlib.Path) -> str:
    return "\n".join(path.read_text(errors="ignore") for path in crate.rglob("*.rs"))


def describe(repo: pathlib.Path) -> str:
    """`<short sha> (<date>)` for a checkout, or an empty string when git cannot say."""
    try:
        out = subprocess.run(
            ["git", "-C", str(repo), "log", "-1", "--format=%h (%ad)", "--date=short"],
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        return ""
    return out.stdout.strip() if out.returncode == 0 else ""


def js_sdk_version(js_sdk: pathlib.Path) -> str:
    """The version the checkout publishes, read from the umbrella package."""
    manifest = js_sdk / "packages" / "firebase" / "package.json"
    try:
        return json.loads(manifest.read_text()).get("version", "")
    except (OSError, ValueError):
        return ""


def measure(js_sdk: pathlib.Path) -> list[dict]:
    """One row per JS package: how many of its public entities have a counterpart here."""
    reports = js_sdk / "common" / "api-review"
    root = pathlib.Path(__file__).resolve().parent.parent

    rows = []
    for package, crate_name, in_total in PAIRS:
        report = reports / f"{package}.api.md"
        crate = root / "crates" / crate_name / "src"
        if not report.exists() or not crate.is_dir():
            continue
        text = crate_text(crate)
        names = entities(report)
        missing = [
            name
            for _kind, name in names
            if not (re.search(rf"\b{re.escape(name)}\b", text) or re.search(rf"\b{re.escape(snake(name))}\b", text))
        ]
        rows.append(
            {
                "package": package,
                "crate": crate_name,
                "total": len(names),
                "matched": len(names) - len(missing),
                "counts_towards_total": in_total,
                "missing": missing,
            }
        )
    return rows


def share(row: dict) -> float:
    return row["matched"] / row["total"] if row["total"] else 0.0


def totals(rows: list[dict]) -> tuple[int, int]:
    counted = [row for row in rows if row["counts_towards_total"]]
    return sum(row["matched"] for row in counted), sum(row["total"] for row in counted)


def print_text(rows: list[dict], with_missing: bool) -> None:
    print(f"{'js package':<18}{'matched':>9}{'total':>7}{'share':>8}   crate")
    for row in rows:
        print(f"{row['package']:<18}{row['matched']:>9}{row['total']:>7}{share(row):>7.0%}   {row['crate']}")
    matched, total = totals(rows)
    print(f"{'all products':<18}{matched:>9}{total:>7}{matched / total:>7.0%}")
    if with_missing:
        for row in rows:
            if row["missing"]:
                print(f"\n--- {row['package']}: {len(row['missing'])} without a counterpart")
                print("    " + ", ".join(row["missing"]))


def print_markdown(rows: list[dict], js_sdk: pathlib.Path, with_missing: bool) -> None:
    """GitHub-flavoured markdown, for `$GITHUB_STEP_SUMMARY`."""
    matched, total = totals(rows)
    version = js_sdk_version(js_sdk)
    revision = describe(js_sdk)
    against = " ".join(part for part in (f"v{version}" if version else "", revision) if part)

    print("## API parity with the Firebase JavaScript SDK")
    print()
    print(f"**{matched} of {total} public entities ({matched / total:.1%})** have a counterpart in this crate.")
    if against:
        print(f"Measured against firebase-js-sdk {against}.")
    print()
    print("| JS package | Rust crate | Matched | Total | Share |")
    print("| --- | --- | ---: | ---: | ---: |")
    for row in rows:
        package = f"`{row['package']}`" + ("" if row["counts_towards_total"] else " *(subset)*")
        print(f"| {package} | `{row['crate']}` | {row['matched']} | {row['total']} | {share(row):.0%} |")
    print(f"| **all products** | | **{matched}** | **{total}** | **{matched / total:.0%}** |")
    print()
    print(
        "Matching is by name and deliberately pessimistic: an API shipped under a more idiomatic "
        "Rust name counts as missing. Treat a number as movement, not as a score — see "
        "[`docs/js-sdk-parity.md`](docs/js-sdk-parity.md)."
    )

    if with_missing:
        print()
        for row in rows:
            if row["missing"]:
                print(f"<details><summary>{row['package']}: {len(row['missing'])} without a counterpart</summary>")
                print()
                print(", ".join(f"`{name}`" for name in row["missing"]))
                print()
                print("</details>")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("js_sdk", type=pathlib.Path, help="path to a firebase-js-sdk checkout")
    parser.add_argument("--missing", action="store_true", help="list the entities with no counterpart")
    parser.add_argument(
        "--format",
        choices=("text", "markdown", "json"),
        default="text",
        help="text for a terminal, markdown for a GitHub step summary, json for tracking over time",
    )
    args = parser.parse_args()

    if not (args.js_sdk / "common" / "api-review").is_dir():
        print(f"no API reports under {args.js_sdk / 'common' / 'api-review'}", file=sys.stderr)
        return 1

    rows = measure(args.js_sdk)
    if not rows:
        print("no packages matched; is this a firebase-js-sdk checkout?", file=sys.stderr)
        return 1

    if args.format == "json":
        print(
            json.dumps(
                {
                    "js_sdk_version": js_sdk_version(args.js_sdk),
                    "js_sdk_revision": describe(args.js_sdk),
                    "matched": totals(rows)[0],
                    "total": totals(rows)[1],
                    "packages": rows,
                },
                indent=2,
            )
        )
    elif args.format == "markdown":
        print_markdown(rows, args.js_sdk, args.missing)
    else:
        print_text(rows, args.missing)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
