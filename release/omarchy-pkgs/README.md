# twr-desktop-bin for Omarchy's package repository

A recipe for [omacom/omarchy-pkgs](https://github.com/omacom/omarchy-pkgs), the repository
Omarchy's pacman installs from. If it is accepted, Omarchy users get the desktop app with
`pacman -S twr-desktop-bin`, it updates with Omarchy's **Update**, and Omarchy's Install menu
can list it. It has not been submitted.

`pkgbuilds/twr-desktop-bin/` mirrors that repository's layout, so it copies straight in:

- `PKGBUILD` installs the release's `twr-desktop-<version>-linux-x64.tar.gz`, the archive the
  AUR recipe (`clients/packaging/aur/PKGBUILD.in`) installs, with the same dependencies. Their
  build requires every file to be owned by root, so it unpacks with `bsdtar --no-same-owner`.
- `.omarchy/package.json` points their release bot at `ignaval/twr`'s GitHub releases, so it
  opens the version and checksum bumps itself. `min_release_age` is 24h because the desktop
  archive is attached some time after the CLI's files.

Nothing in twr's release pipeline touches it. Keep its `depends` and `optdepends` in step with
the AUR template when those change.

## Submitting it

1. Fork omacom/omarchy-pkgs and branch from `master`.
2. If a release newer than the recipe's `pkgver` is out, set `pkgver` to it and `sha256sums`
   to the archive's line in that release's `desktop-sha256sums.txt` (check its signature first).
3. Copy `pkgbuilds/twr-desktop-bin/` in, and run their checks on an Arch machine:
   `tests/pinned-sources.sh`, `tests/package-metadata.sh` (needs `jq`), `tests/pkgname-unique.sh`,
   `tests/pgp-keys-vendored.sh`, `tests/package-owners.sh`, and
   `python helpers/upstream-watch.py check pkgbuilds/twr-desktop-bin`, which should report the
   release as current. Then `makepkg` in the package's folder, and `pacman -U` the result.
4. Open the PR **as a draft**, with `PR.md` as its text. Their AGENTS.md says a non-draft PR
   that only touches `pkgbuilds/` merges and publishes itself once its checks pass. A maintainer
   adds the `build-approved` label before their CI builds it.

Checked so far (v0.4.0, Arch x86_64): every test above passes, the watch reports 0.4.0 as
current, `makepkg` builds it, their `check_package_owners` accepts it, and it installs with
`pacman -Qkk` clean and `chrome-sandbox` setuid root. Their Docker build (`bin/build`) has not
been run.

## Once it is in

- Add an Install › AI row to Omarchy's menu (`default/omarchy/omarchy-menu.jsonc` in
  omacom/omarchy): `omarchy-install-and-launch twr twr-desktop-bin tools.twr.desktop`.
- The app's update notice for pacman installs (`manualInstructions` in
  `clients/desktop/update-policy.ts`) names an AUR helper and `pacman -U`; for this package,
  Omarchy's system update is the answer.
