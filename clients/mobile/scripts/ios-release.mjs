// Archives the iOS app's Release configuration for the App Store (TestFlight), from clients/.
// Run `npm run mobile:build:store` first (npm run mobile:release:ios does both).
//
//   node mobile/scripts/ios-release.mjs --check    archive without signing: no Apple account,
//                                                  proves the device build of Release compiles
//   node mobile/scripts/ios-release.mjs            archive, sign and export an .ipa
//   node mobile/scripts/ios-release.mjs --upload   the same, and send it to App Store Connect
//
// Signing is Xcode's automatic signing for the team in TWR_IOS_TEAM_ID. On a Mac signed in to
// Xcode with that team nothing else is needed. Elsewhere (CI), an App Store Connect API key
// signs and uploads: TWR_ASC_KEY_PATH (the AuthKey_<id>.p8 file), TWR_ASC_KEY_ID and
// TWR_ASC_ISSUER_ID. The build number is build-number.mjs's. Output: mobile/ios/App/output/.
// docs/mobile.md, Publishing.
import { execFileSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { isAbsolute, resolve } from "node:path";
import { clientsVersion, currentBuildNumber } from "./build-number.mjs";

const check = process.argv.includes("--check");
const upload = process.argv.includes("--upload");
const out = resolve("mobile/ios/App/output");
const archive = `${out}/twr.xcarchive`;

function xcodebuild(args) {
  execFileSync("xcodebuild", args, { stdio: "inherit" });
}

try {
  if (check && upload)
    throw new Error("--check and --upload do not go together");
  const sdk = execFileSync(
    "xcrun",
    ["--sdk", "iphoneos", "--show-sdk-version"],
    {
      encoding: "utf8",
    },
  )
    .trim()
    .split(".")
    .map(Number);
  if (sdk[0] < 27 || (sdk[0] === 27 && (sdk[1] || 0) < 1))
    throw new Error("the iOS SDK 27.1 or newer is required (Xcode 27.1)");

  const team = process.env.TWR_IOS_TEAM_ID || "";
  if (!check && !/^[A-Z0-9]{10}$/.test(team))
    throw new Error(
      "set TWR_IOS_TEAM_ID to your Apple Developer team id (10 letters and digits), or pass --check",
    );
  const key = {
    path: process.env.TWR_ASC_KEY_PATH || "",
    id: process.env.TWR_ASC_KEY_ID || "",
    issuer: process.env.TWR_ASC_ISSUER_ID || "",
  };
  const keyed = Boolean(key.path || key.id || key.issuer);
  if (keyed && !(key.path && key.id && key.issuer))
    throw new Error(
      "an App Store Connect API key needs TWR_ASC_KEY_PATH, TWR_ASC_KEY_ID and TWR_ASC_ISSUER_ID together",
    );
  if (keyed && !isAbsolute(key.path))
    throw new Error("TWR_ASC_KEY_PATH must be an absolute path");
  // With these, xcodebuild makes and renews the distribution certificate and profile itself.
  const auth = check
    ? []
    : [
        "-allowProvisioningUpdates",
        ...(keyed
          ? [
              "-authenticationKeyPath",
              key.path,
              "-authenticationKeyID",
              key.id,
              "-authenticationKeyIssuerID",
              key.issuer,
            ]
          : []),
      ];

  const version = clientsVersion();
  const build = currentBuildNumber();
  console.log(
    `twr ${version} (${build}) for iOS${check ? ", unsigned check" : ""}`,
  );
  rmSync(out, { recursive: true, force: true });
  mkdirSync(out, { recursive: true });
  xcodebuild([
    "-project",
    "mobile/ios/App/App.xcodeproj",
    "-scheme",
    "App",
    "-configuration",
    "Release",
    "-destination",
    "generic/platform=iOS",
    "-derivedDataPath",
    `${out}/DerivedData`,
    "-archivePath",
    archive,
    `MARKETING_VERSION=${version.match(/^\d+\.\d+\.\d+/)[0]}`,
    `CURRENT_PROJECT_VERSION=${build}`,
    ...(check ? ["CODE_SIGNING_ALLOWED=NO"] : [`DEVELOPMENT_TEAM=${team}`]),
    ...auth,
    "archive",
  ]);
  if (check) {
    console.log(`Archived without signing: ${archive}`);
  } else {
    const options = `${out}/ExportOptions.plist`;
    writeFileSync(
      options,
      `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>method</key><string>app-store-connect</string>
  <key>destination</key><string>${upload ? "upload" : "export"}</string>
  <key>teamID</key><string>${team}</string>
  <key>signingStyle</key><string>automatic</string>
  <key>manageAppVersionAndBuildNumber</key><false/>
  <key>uploadSymbols</key><true/>
</dict></plist>
`,
    );
    xcodebuild([
      "-exportArchive",
      "-archivePath",
      archive,
      "-exportPath",
      out,
      "-exportOptionsPlist",
      options,
      ...auth,
    ]);
    console.log(
      upload
        ? `Uploaded twr ${version} (${build}) to App Store Connect; it appears in TestFlight once processed.`
        : `Exported to ${out}`,
    );
  }
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
