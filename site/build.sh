#!/bin/sh
# Builds twr.tools into site-dist/: the page, the installer and the images the page shows.
# vercel.json's buildCommand runs it from the repository's root (Vercel allows that command
# 256 characters, too few for the list). It fails when the page refers to a file it doesn't
# copy, so an image added to the page can't go live missing.
set -eu
cd "$(dirname "$0")/.."
sh -n install.sh
rm -rf site-dist
mkdir site-dist
cp site/index.html install.sh site-dist/
for img in tui-conversation tui-conversation-still tui-conversation-phone tui-conversation-phone-still; do
    cp "docs/img/$img.svg" site-dist/
done
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
