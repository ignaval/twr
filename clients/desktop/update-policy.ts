/**
 * How this copy of twr was installed decides how it updates. Only formats
 * whose updater can replace the application without a package manager update
 * themselves; packages stay owned by their package manager.
 */
import type { InstallKind, UpdateStatus } from "../shared/types";
export type { InstallKind, UpdateStatus };

export interface ReleaseAsset {
  name: string;
  url: string;
}

export function detectInstallKind(input: {
  platform: NodeJS.Platform;
  packaged: boolean;
  env: NodeJS.ProcessEnv;
  execPath: string;
  packageType?: string;
}): InstallKind {
  if (!input.packaged) return "development";
  if (input.platform === "darwin") return "mac";
  if (input.platform !== "linux") return "portable";
  if (input.env.APPIMAGE) return "appimage";
  // electron-builder writes resources/package-type for its deb/rpm/pacman targets.
  if (input.packageType === "deb") return "deb";
  if (input.packageType === "pacman") return "pacman";
  // electron-builder's Debian layout, when the marker is missing.
  if (input.execPath.startsWith("/opt/twr/")) return "deb";
  if (input.execPath.startsWith("/opt/twr-desktop/")) return "pacman";
  return "portable";
}

/** `codesign --display --verbose=2` output for a Developer ID signature. */
export function developerIdSigned(codesignOutput: string) {
  return (
    /^Authority=Developer ID Application: /m.test(codesignOutput) &&
    /^TeamIdentifier=[A-Z0-9]{10}$/m.test(codesignOutput)
  );
}

const VERSION = /^v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?$/;

/** Semantic-version ordering (major.minor.patch, then prerelease identifiers). */
export function compareVersions(a: string, b: string) {
  const left = VERSION.exec(a.trim());
  const right = VERSION.exec(b.trim());
  if (!left || !right) throw new Error(`Cannot compare versions ${a} and ${b}`);
  for (let i = 1; i <= 3; i++) {
    const difference = Number(left[i]) - Number(right[i]);
    if (difference) return Math.sign(difference);
  }
  if (!left[4] || !right[4]) return left[4] ? -1 : right[4] ? 1 : 0;
  const l = left[4].split(".");
  const r = right[4].split(".");
  for (let i = 0; i < Math.max(l.length, r.length); i++) {
    if (l[i] === undefined) return -1;
    if (r[i] === undefined) return 1;
    const ln = /^\d+$/.test(l[i]);
    const rn = /^\d+$/.test(r[i]);
    if (ln && rn) {
      const difference = Number(l[i]) - Number(r[i]);
      if (difference) return Math.sign(difference);
    } else if (ln !== rn) return ln ? -1 : 1;
    else if (l[i] !== r[i]) return l[i] < r[i] ? -1 : 1;
  }
  return 0;
}

/** The update metadata electron-updater reads for each self-updating kind. */
export function updateMetadataName(kind: InstallKind) {
  if (kind === "mac") return "latest-mac.yml";
  if (kind === "appimage") return "latest-linux.yml";
  return undefined;
}

export function installerFor(
  kind: InstallKind,
  assets: ReleaseAsset[],
): ReleaseAsset | undefined {
  const pattern =
    kind === "mac"
      ? /-arm64\.dmg$/
      : kind === "appimage"
        ? /\.AppImage$/
        : kind === "deb"
          ? /_amd64\.deb$/
          : kind === "pacman"
            ? /-x86_64\.pkg\.tar\.zst$/
            : kind === "portable"
              ? /-linux-x64\.tar\.gz$/
              : undefined;
  return pattern && assets.find((asset) => pattern.test(asset.name));
}

export function manualInstructions(
  kind: InstallKind,
  download: ReleaseAsset | undefined,
) {
  switch (kind) {
    case "mac":
      return "Download the disk image, quit twr, and drag the new twr into Applications.";
    case "deb":
      return download
        ? `Download the package, then run: sudo apt install ./${download.name}`
        : "Install the new Debian package from the release page.";
    case "pacman":
      return "Update with your AUR helper (for example: yay -Syu twr-desktop-bin), or install the package from the release page with sudo pacman -U.";
    case "portable":
      return "Download the new archive from the release page and replace this copy.";
    case "development":
      return "This is a development build. Pull and rebuild to update.";
    default:
      return undefined;
  }
}

export const UPDATE_CHECK_INTERVAL = 6 * 60 * 60 * 1000;
export const FIRST_UPDATE_CHECK_DELAY = 45 * 1000;
