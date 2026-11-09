#!/bin/sh
# Builds twr.tools into site-dist/: the page, the installer, the images the page shows and the
# icon its link previews show. vercel.json's buildCommand runs it from the repository's root
# (Vercel allows that command 256 characters, too few for the list). It fails when the page
# refers to a file it doesn't copy, so an image added to the page can't go live missing, and
# when vercel.json's Content-Security-Policy doesn't allow the page's inline script by its hash,
# so a changed script can't go live blocked.
set -eu
cd "$(dirname "$0")/.."
sh -n install.sh
rm -rf site-dist
mkdir site-dist
cp site/index.html install.sh site-dist/
for img in tui-conversation tui-conversation-still tui-conversation-phone tui-conversation-phone-still; do
    cp "docs/img/$img.svg" site-dist/
done
# og:image and twitter:card (the desktop app's icon).
cp clients/packaging/icon.png site-dist/icon.png
# Every src, srcset, data-still and same-site href in the page names a file in site-dist/.
grep -o -E '(src|srcset|data-still|href)="[^"#:]+"' site/index.html |
    sed -e 's/^[a-z-]*="//' -e 's/"$//' | grep -v '^/$' | sort -u > site-dist/.refs
missing=0
while read -r f; do
    if [ ! -f "site-dist/${f#/}" ]; then
        echo "site/build.sh: site/index.html refers to $f, which the build doesn't copy" >&2
        missing=1
    fi
done < site-dist/.refs
rm site-dist/.refs
[ "$missing" -eq 0 ]
# Each inline <script> of the page, by the SHA-256 the browser hashes it with, must be in
# vercel.json's script-src; the page has no other scripts.
command -v node > /dev/null 2>&1 || { echo "site/build.sh: needs node to hash the page's script" >&2; exit 1; }
# shellcheck disable=SC2016 # JavaScript, not shell.
node -e '
const fs = require("fs");
const crypto = require("crypto");
const page = fs.readFileSync("site/index.html", "utf8");
const csp = JSON.parse(fs.readFileSync("vercel.json", "utf8")).headers
  .flatMap((rule) => rule.headers)
  .find((h) => h.key === "Content-Security-Policy").value;
let ok = true;
if (/<script\b[^>]*\bsrc=/i.test(page)) {
  console.error("site/build.sh: site/index.html loads a script file; the CSP allows inline scripts by hash only");
  ok = false;
}
for (const m of page.matchAll(/<script>([\s\S]*?)<\/script>/g)) {
  const hash = "sha256-" + crypto.createHash("sha256").update(m[1], "utf8").digest("base64");
  if (!csp.includes(`\x27${hash}\x27`)) {
    console.error(`site/build.sh: vercel.json script-src lacks \x27${hash}\x27, the hash of the page\x27s inline script; put it in place of the old one`);
    ok = false;
  }
}
process.exit(ok ? 0 : 1);
'
