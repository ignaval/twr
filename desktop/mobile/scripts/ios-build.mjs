import { existsSync } from "node:fs";
import { execFileSync } from "node:child_process";
try {
  const push = process.argv.includes("--push");
  const sdk = push ? "iphoneos" : "iphonesimulator";
  if (push && !existsSync("mobile/ios/push.local.xcconfig"))
    throw new Error(
      "Copy push.local.example.xcconfig to push.local.xcconfig and set your push-capable Apple team first.",
    );
  const version = execFileSync("xcrun", ["--sdk", sdk, "--show-sdk-version"], {
    encoding: "utf8",
  })
    .trim()
    .split(".")
    .map(Number);
  if (version[0] < 27 || (version[0] === 27 && (version[1] || 0) < 1))
    throw new Error(
      "iOS Simulator SDK 27.1 or newer is required for native folding-display support.",
    );
  execFileSync(
    "xcodebuild",
    [
      "-project",
      "mobile/ios/App/App.xcodeproj",
      "-scheme",
      "App",
      "-configuration",
      "Debug",
      "-sdk",
      sdk,
      "-derivedDataPath",
      "mobile/ios/App/DerivedData",
      ...(push
        ? [
            "-xcconfig",
            "mobile/ios/push.local.xcconfig",
            "-destination",
            "generic/platform=iOS",
          ]
        : ["CODE_SIGNING_ALLOWED=NO"]),
      "build",
    ],
    { stdio: "inherit" },
  );
} catch (error) {
  console.error(
    "Install and open Xcode 27.1 or newer, then install an iOS Simulator runtime.",
  );
  console.error(error.message);
  process.exitCode = 1;
}
