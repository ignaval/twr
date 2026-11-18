#!/usr/bin/env python3
"""`npm audit --audit-level=high` for clients/, less the advisories listed in ALLOWED.

    cd clients && python3 ../.github/scripts/npm-audit.py

It runs `npm audit --json` in the current directory and fails (exit status 1) when an advisory
of high or critical severity that ALLOWED doesn't name affects any package of the tree,
devDependencies included: Electron and the Capacitor runtime ship in the apps but are
devDependencies, so `--omit=dev` would skip them. An allowed advisory is printed as a notice;
one npm no longer reports is printed as a warning, so its entry gets removed.

An advisory belongs in ALLOWED only when no fixed version exists and the code it affects does
not ship in the apps, with that reason next to it. Remove the entry once a fix is out and the
lockfile has it. Standard library only.
"""

import json
import subprocess
import sys

# Advisory URL -> why it is allowed for now.
ALLOWED = {
    "https://github.com/advisories/GHSA-fc5q-39xc-vjpf": (
        "picomatch ReDoS: no fixed version yet (every version up to 4.0.7 is affected); it "
        "reaches clients/ only through build tools (vite, electron-builder, node-gyp), so it "
        "does not ship in the apps"
    ),
}

FAILING = ("high", "critical")


def main() -> int:
    run = subprocess.run(["npm", "audit", "--json"], capture_output=True, text=True)
    try:
        report = json.loads(run.stdout)
    except json.JSONDecodeError:
        sys.stderr.write(run.stderr)
        print("::error::npm audit did not answer with JSON")
        return 1
    if "error" in report or "vulnerabilities" not in report:
        print(f"::error::npm audit failed: {json.dumps(report.get('error', report))[:500]}")
        return 1
    # The advisories themselves: a package that is only vulnerable through another names that
    # package in `via` (a string); the advisory is an object in the affected package's `via`.
    advisories = {}
    for name, vuln in report["vulnerabilities"].items():
        for via in vuln.get("via", []):
            if isinstance(via, dict) and via.get("url"):
                entry = advisories.setdefault(
                    via["url"], (via.get("severity", ""), via.get("title", ""), set())
                )
                entry[2].add(name)
    failed = False
    for url, (severity, title, packages) in sorted(advisories.items()):
        if severity not in FAILING:
            continue
        where = ", ".join(sorted(packages))
        if url in ALLOWED:
            print(f"::notice::allowed {severity} advisory {url} ({where}): {ALLOWED[url]}")
        else:
            print(f"::error::{severity} advisory {url} ({where}): {title}")
            failed = True
    for url in sorted(set(ALLOWED) - set(advisories)):
        print(f"::warning::{url} is allowed in .github/scripts/npm-audit.py but npm audit no "
              "longer reports it: remove its entry")
    counts = report.get("metadata", {}).get("vulnerabilities", {})
    print(f"npm audit: {counts}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
