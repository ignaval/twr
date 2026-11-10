// electron-builder's afterPack hook, before any installer is made from the packed app.
import { existsSync } from "node:fs";
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
  const missing = REQUIRED_RESOURCES.filter(
    (name) => !existsSync(join(resources, name)),
  );
  if (missing.length)
    throw new Error(
      `The packed app lacks ${missing.join(", ")} in ${resources}. Package with npm run package (it runs scripts/notices.mjs).`,
    );
  return linuxAbi(context);
}
