// The build number both stores ask to grow with every upload: Android's versionCode and iOS's
// CFBundleVersion. It is made from the clients' version (package.json) and from
// TWR_MOBILE_BUILD_NUMBER (0 to 99, for another upload of the same version): 0.4.0 build 0 is
// 40000, 1.2.3 build 4 is 1020304. mobile/android/app/build.gradle computes the same number.
//
//   node mobile/scripts/build-number.mjs     prints it (run from clients/)
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

export function buildNumber(version, build = "0") {
  const parts = /^(\d+)\.(\d+)\.(\d+)/.exec(version);
  if (!parts) throw new Error(`version ${version} is not major.minor.patch`);
  if (!/^\d{1,2}$/.test(build))
    throw new Error(`TWR_MOBILE_BUILD_NUMBER must be 0 to 99, not '${build}'`);
  const [major, minor, patch] = parts.slice(1).map(Number);
  if (minor > 99 || patch > 99)
    throw new Error(
      `version ${version}: minor and patch must be under 100 for the build number`,
    );
  return major * 1000000 + minor * 10000 + patch * 100 + Number(build);
}

export function clientsVersion() {
  const file = new URL("../../package.json", import.meta.url);
  return JSON.parse(readFileSync(file, "utf8")).version;
}

export const currentBuildNumber = () =>
  buildNumber(clientsVersion(), process.env.TWR_MOBILE_BUILD_NUMBER || "0");

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  try {
    console.log(currentBuildNumber());
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
