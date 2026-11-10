// electron-builder's afterPack hook, before the app is signed and any installer is made.
import { existsSync, rmSync } from "node:fs";
import { join } from "node:path";
import linuxAbi from "./linux-abi.mjs";

/** The files every desktop package must carry in its resources folder. */
export const REQUIRED_RESOURCES = [
  "LICENSE",
  "NOTICE",
  // scripts/notices.mjs; electron-builder skips a missing extraResources file silently.
  "THIRD-PARTY-LICENSES.html",
];

export default function afterPack(context) {
  const mac = context.electronPlatformName === "darwin";
  const resources = mac
    ? join(
        context.appOutDir,
        `${context.packager.appInfo.productFilename}.app`,
        "Contents/Resources",
      )
    : join(context.appOutDir, "resources");
  const missing = [
    ...REQUIRED_RESOURCES,
    ...(mac ? [] : ["bin/digests.json"]),
  ].filter((name) => !existsSync(join(resources, name)));
  if (missing.length)
    throw new Error(
      `The packed app lacks ${missing.join(", ")} in ${resources}. Package with npm run package (it runs scripts/notices.mjs).`,
    );
  // Signing the Mac app re-signs its engines, so the digests scripts/build-rust.mjs took
  // would never match them, and every launch would copy and hash the engines once for
  // nothing before falling back (desktop/engines.ts). The bundle's path is stable, so the
  // app's own digest cache serves instead; the manifest is for AppImages, which mount at a
  // new path on every launch.
  if (mac) rmSync(join(resources, "bin/digests.json"), { force: true });
  return linuxAbi(context);
}
