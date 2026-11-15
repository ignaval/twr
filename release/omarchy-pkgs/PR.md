Title: Add twr-desktop-bin

twr (https://github.com/ignaval/twr) runs several coding agents (Claude Code, Codex, Grok) from one conversation, in a terminal UI or this desktop app. This adds the desktop app as `twr-desktop-bin`, x86_64 only (there is no aarch64 Linux build yet).

The recipe installs the archive each stable twr release publishes, `twr-desktop-<version>-linux-x64.tar.gz`: the Electron app with its native terminal module and the twr engines, built on the Ubuntu 22.04 ABI. twr's own Arch package installs the same archive, so the files match. Chromium's sandbox stays on (`chrome-sandbox` is setuid root), and the launcher passes the window class `tools.twr.desktop`, which is also the desktop file's name. The dependencies are what namcap finds the bundle's ELF files link, plus `git`, which twr runs, `libsecret`, which Chromium loads for the keyring, and `hicolor-icon-theme` for the icon. It provides and conflicts with `twr-desktop`, the name of twr's own release package.

`.omarchy/package.json` watches `ignaval/twr` GitHub releases. Stable releases attach the desktop archive some time after the CLI's files, so `min_release_age` is 24h. I set the fast ring, as for the other apps people install from the menu; happy to drop it.

Tested on Arch x86_64:
- `tests/pinned-sources.sh`, `package-metadata.sh`, `pkgname-unique.sh`, `pgp-keys-vendored.sh` and `package-owners.sh` pass.
- `python helpers/upstream-watch.py check pkgbuilds/twr-desktop-bin` reports `already current` (0.4.0).
- `makepkg` builds it, `check_package_owners` accepts it, and it installs with `pacman -U`: `pacman -Qkk` clean, `chrome-sandbox` 4755 root, `desktop-file-validate` passes.
- I couldn't run `bin/build` (no Docker on this machine).

namcap's errors: ELF files under `/opt` (a self-contained Electron bundle), and, for 0.4.0 only, a `python` dependency from node-pty's Windows sources in that archive, which twr's next release leaves out.
