#!/usr/bin/env python3
"""Writes the Homebrew tap's formula and cask (ignaval/homebrew-twr) for one release.

    python3 -I release/homebrew/render.py SHA256SUMS DESKTOP_SHA256SUMS OUT

SHA256SUMS and DESKTOP_SHA256SUMS are the release's sha256sums.txt and desktop-sha256sums.txt,
whose signatures the caller has checked (release.yml does). The version and every checksum come
from them. OUT receives Formula/twr.rb (the CLI) and, when the release's Mac app is Developer
ID signed and notarized, Casks/twr.rb (the desktop app): Homebrew quarantines what a cask
installs, so Gatekeeper would stop an ad hoc app, and only a signed release carries
latest-mac.yml (the self-update metadata desktop.yml publishes for Developer ID builds alone).
It prints what it wrote, and exits non-zero when the lists are not one stable release's.
"""

import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
TARGETS = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-musl",
]


def read_list(path, prefix):
    """The release tag a checksum list names on its `prefix` line, and its {file: sha256}."""
    tags, sums = [], {}
    for line in Path(path).read_text().splitlines():
        if line.startswith(prefix):
            tags.append(line[len(prefix):])
            continue
        match = re.fullmatch(r"([0-9a-f]{64}) [ *]([^/\s]+)", line)
        if match:
            sums[match.group(2)] = match.group(1)
        elif line.strip():
            raise SystemExit(f"{path}: unexpected line {line!r}")
    if len(tags) != 1:
        raise SystemExit(f"{path} does not name one release (a '{prefix}<tag>' line)")
    return tags[0], sums


def fill(template, values):
    text = (HERE / template).read_text()
    for key, value in values.items():
        text = text.replace(f"@{key}@", value)
    left = re.findall(r"@[A-Z0-9_]+@", text)
    if left:
        raise SystemExit(f"{template}: nothing fills {', '.join(sorted(set(left)))}")
    return text


def main(argv):
    if len(argv) != 4:
        raise SystemExit(__doc__.strip().splitlines()[2].strip())
    _, cli_list, desktop_list, out = argv
    tag, cli = read_list(cli_list, "# twr release ")
    desktop_tag, desktop = read_list(desktop_list, "# twr desktop release ")
    if desktop_tag != tag:
        raise SystemExit(f"{cli_list} is {tag}'s, but {desktop_list} is {desktop_tag}'s")
    if not re.fullmatch(r"v\d+\.\d+\.\d+", tag):
        raise SystemExit(f"{tag} is not a stable release; the tap follows stable releases only")
    version = tag[1:]
    out = Path(out)

    values = {"VERSION": version}
    for target in TARGETS:
        name = f"twr-{target}.tar.gz"
        if name not in cli:
            raise SystemExit(f"{cli_list} does not list {name}")
        values["SHA256_" + target.upper().replace("-", "_")] = cli[name]
    (out / "Formula").mkdir(parents=True, exist_ok=True)
    (out / "Formula/twr.rb").write_text(fill("twr.formula.rb.in", values))
    print(f"wrote {out / 'Formula/twr.rb'} (twr {version})")

    dmg = f"twr-{version}-arm64.dmg"
    if dmg not in desktop:
        raise SystemExit(f"{desktop_list} does not list {dmg}")
    if "latest-mac.yml" not in desktop:
        print(
            f"no cask: {tag}'s Mac app is not Developer ID signed and notarized "
            "(the release has no latest-mac.yml)"
        )
        return
    (out / "Casks").mkdir(parents=True, exist_ok=True)
    (out / "Casks/twr.rb").write_text(
        fill("twr.cask.rb.in", {"VERSION": version, "SHA256_DMG": desktop[dmg]})
    )
    print(f"wrote {out / 'Casks/twr.rb'} (the twr app {version})")


if __name__ == "__main__":
    main(sys.argv)
