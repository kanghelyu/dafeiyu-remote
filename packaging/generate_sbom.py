#!/usr/bin/env python3
# SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
"""Audit workspace/third-party dependency licenses and generate a CycloneDX 1.5 SBOM."""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import re
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
EXPECTED_WORKSPACE_LICENSE = "LicenseRef-PolyForm-Noncommercial-1.0.0"

ALLOWED_LICENSES = {
    "0BSD",
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "BSL-1.0",
    "CC0-1.0",
    "CDLA-Permissive-2.0",
    "IJG",
    "ISC",
    "MIT",
    "MPL-2.0",
    "OpenSSL",
    "Unicode-3.0",
    "Unicode-DFS-2016",
    "Unlicense",
    "Zlib",
    "bzip2-1.0.6",
}

FORBIDDEN_TOKENS = ("GPL", "AGPL", "LGPL", "SSPL")


def split_spdx_tokens(expr: str) -> list[str]:
    cleaned = expr.replace("(", " ").replace(")", " ").replace("/", " OR ")
    tokens = []
    for part in re.split(r"\s+(?:OR|AND|WITH)\s+|\s+", cleaned):
        part = part.strip()
        if part and part not in {"OR", "AND", "WITH"}:
            tokens.append(part)
    return tokens


def is_license_allowed(expr: str | None) -> tuple[bool, str]:
    if not expr or not expr.strip():
        return False, "missing license expression"
    tokens = split_spdx_tokens(expr)
    # For pure 'A OR B OR C' expressions, any single allowed license suffices.
    if " OR " in expr and " AND " not in expr:
        if any(tok in ALLOWED_LICENSES for tok in tokens):
            return True, "ok"
        return False, f"no allowed license in OR expression '{expr}'"
    for token in tokens:
        upper = token.upper()
        if any(bad in upper for bad in FORBIDDEN_TOKENS):
            return False, f"forbidden copyleft token in '{expr}'"
    # Handle '(MIT OR Apache-2.0) AND IJG' style expressions:
    # split top-level AND clauses; each clause must be satisfied.
    for clause in re.split(r"\s+AND\s+", expr):
        clause_tokens = split_spdx_tokens(clause)
        if " OR " in clause:
            if not any(tok in ALLOWED_LICENSES for tok in clause_tokens):
                return False, f"no allowed license in clause '{clause}' of '{expr}'"
        else:
            unknown = [tok for tok in clause_tokens if tok not in ALLOWED_LICENSES]
            if unknown:
                return False, f"unreviewed license token(s) {unknown} in '{expr}'"
    return True, "ok"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--metadata-json", type=Path, help="Pre-generated cargo metadata JSON")
    parser.add_argument("--output-sbom", type=Path, help="Write CycloneDX 1.5 JSON here")
    parser.add_argument("--output-notices", type=Path, help="Write THIRD-PARTY-NOTICES.txt here")
    parser.add_argument("--check", action="store_true", help="Fail if any workspace or dependency license fails audit")
    args = parser.parse_args()

    if args.metadata_json:
        meta = json.loads(args.metadata_json.read_text(encoding="utf-8"))
    else:
        proc = subprocess.run(
            ["cargo", "metadata", "--locked", "--format-version", "1"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=True,
        )
        meta = json.loads(proc.stdout)

    workspace_ids = set(meta.get("workspace_members", []))
    packages = meta.get("packages", [])
    failures: list[str] = []
    components: list[dict[str, object]] = []
    notices: list[str] = [
        "Dafeiyu Remote - Third-Party Dependency Notices",
        "================================================",
        "",
    ]

    for pkg in sorted(packages, key=lambda p: (p["name"], p["version"])):
        name = pkg["name"]
        version = pkg["version"]
        lic = pkg.get("license")
        is_ws = pkg["id"] in workspace_ids
        if is_ws:
            if lic != EXPECTED_WORKSPACE_LICENSE:
                failures.append(f"workspace crate {name} has license {lic!r}, expected {EXPECTED_WORKSPACE_LICENSE!r}")
        else:
            ok, reason = is_license_allowed(lic)
            if not ok:
                failures.append(f"dependency {name} {version}: {reason}")
            notices.append(f"- {name} {version}: {lic or 'UNKNOWN'}")
            if pkg.get("repository"):
                notices.append(f"  repo: {pkg['repository']}")

        comp: dict[str, object] = {
            "type": "library",
            "bom-ref": f"pkg:cargo/{name}@{version}",
            "name": name,
            "version": version,
            "purl": f"pkg:cargo/{name}@{version}",
        }
        if lic:
            comp["licenses"] = [{"expression": lic}]
        components.append(comp)

    app_version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    sbom = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {
            "timestamp": datetime(2026, 11, 27, 0, 0, 0, tzinfo=timezone.utc).isoformat(),
            "component": {
                "type": "application",
                "name": "dafeiyu-remote",
                "version": app_version,
                "licenses": [{"expression": EXPECTED_WORKSPACE_LICENSE}],
            },
        },
        "components": components,
    }

    if args.output_sbom:
        args.output_sbom.parent.mkdir(parents=True, exist_ok=True)
        args.output_sbom.write_text(json.dumps(sbom, indent=2) + "\n", encoding="utf-8", newline="\n")
    if args.output_notices:
        args.output_notices.parent.mkdir(parents=True, exist_ok=True)
        args.output_notices.write_text("\n".join(notices) + "\n", encoding="utf-8", newline="\n")

    if failures:
        for item in failures:
            print(f"AUDIT FAIL: {item}", file=sys.stderr)
        if args.check:
            sys.exit(1)

    print(
        f"SBOM AUDIT PASS: {len(workspace_ids)} workspace crates ({EXPECTED_WORKSPACE_LICENSE}), "
        f"{len(packages) - len(workspace_ids)} third-party dependencies audited"
    )


if __name__ == "__main__":
    main()
