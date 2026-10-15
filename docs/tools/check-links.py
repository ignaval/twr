#!/usr/bin/env python3
"""Check the relative links in README.md, SKILL.md, the other top-level Markdown files
(CONTRIBUTING.md, SECURITY.md, ...) and docs/*.md.

Usage: python3 docs/tools/check-links.py

Every link that isn't a URL must point at an existing file, and a `#fragment` must match a
heading of the target, slugged the way GitHub does it. Links inside code blocks and code spans
are ignored. Exits 1 and lists the broken links if there are any. Standard library only.
"""

import os
import re
import sys

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
LINK = re.compile(r"!?\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
HEADING = re.compile(r"^(#{1,6})\s+(.*?)\s*#*\s*$")


def strip_code(text):
    """The text with fenced blocks and inline code blanked out (line numbers kept)."""
    out, fenced = [], False
    for line in text.split("\n"):
        if line.lstrip().startswith("```"):
            fenced = not fenced
            out.append("")
            continue
        out.append("" if fenced else re.sub(r"`[^`]*`", "", line))
    return "\n".join(out)


def slug(heading):
    s = heading.strip().lower()
    s = re.sub(r"[`*]|\[([^\]]*)\]\([^)]*\)", lambda m: m.group(1) or "", s)
    s = re.sub(r"[^\w\- ]", "", s)
    return s.replace(" ", "-")


def anchors(path):
    found, counts, fenced = set(), {}, False
    with open(path, encoding="utf-8") as f:
        for line in f:
            if line.lstrip().startswith("```"):
                fenced = not fenced
                continue
            m = None if fenced else HEADING.match(line.rstrip("\n"))
            if m:
                base = slug(m.group(2))
                n = counts.get(base, 0)
                counts[base] = n + 1
                found.add(base if n == 0 else f"{base}-{n}")
    return found


def files():
    for n in sorted(os.listdir(ROOT)):
        if n.endswith(".md"):
            yield os.path.join(ROOT, n)
    docs = os.path.join(ROOT, "docs")
    for dirpath, _, names in os.walk(docs):
        for n in sorted(names):
            if n.endswith(".md"):
                yield os.path.join(dirpath, n)


def main():
    broken, checked = [], 0
    for src in files():
        with open(src, encoding="utf-8") as f:
            text = strip_code(f.read())
        for lineno, line in enumerate(text.split("\n"), 1):
            for m in LINK.finditer(line):
                target = m.group(1)
                if re.match(r"^[a-z][a-z0-9+.-]*:", target):
                    continue  # https:, mailto:, ...
                checked += 1
                path, _, frag = target.partition("#")
                dest = os.path.normpath(os.path.join(os.path.dirname(src), path)) if path else src
                where = f"{os.path.relpath(src, ROOT)}:{lineno}: {target}"
                if not os.path.exists(dest):
                    broken.append(f"{where}: no such file")
                elif frag and dest.endswith(".md") and frag not in anchors(dest):
                    broken.append(f"{where}: no heading #{frag}")
    for b in broken:
        print(b)
    print(f"{checked} relative links checked, {len(broken)} broken")
    sys.exit(1 if broken else 0)


if __name__ == "__main__":
    main()
