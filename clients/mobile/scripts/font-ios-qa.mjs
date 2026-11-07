// Run the focused native typography XCTest on a disposable Simulator clone.
import { createServer } from "node:http";
import { spawn, execFile } from "node:child_process";
import { promisify } from "node:util";
import assert from "node:assert/strict";
const exec = promisify(execFile);
const device = process.env.TWR_IOS_FONT_DEVICE;
assert(
  device && /^[A-Fa-f0-9-]{36}$/.test(device),
  "Set TWR_IOS_FONT_DEVICE to your disposable QA clone's UUID",
);
const server = createServer(async (request, response) => {
  const url = new URL(request.url, "http://localhost");
  if (url.pathname === "/ready") {
    response.end("ready");
    return;
  }
  const category = url.searchParams.get("category");
  if (
    url.pathname !== "/font" ||
    !["large", "accessibility-extra-large"].includes(category)
  ) {
    response.statusCode = 400;
    response.end();
    return;
  }
  try {
    await exec("xcrun", ["simctl", "ui", device, "content_size", category]);
    response.end("ok");
  } catch {
    response.statusCode = 500;
    response.end("Could not change QA clone's content size");
  }
});
await new Promise((resolve) => server.listen(47330, "127.0.0.1", resolve));
try {
  const command = spawn(
    "xcodebuild",
    [
      "test",
      "-project",
      "mobile/ios/App/App.xcodeproj",
      "-scheme",
      "App",
      "-configuration",
      "Debug",
      "-destination",
      `platform=iOS Simulator,id=${device}`,
      "-derivedDataPath",
      "mobile/ios/App/DerivedData",
      "-packageAuthorizationProvider",
      "netrc",
      "-disableAutomaticPackageResolution",
      "-onlyUsePackageVersionsFromResolvedFile",
      "-parallel-testing-enabled",
      "NO",
      "-only-testing:AppTests/RemoteHubTests/testNativeTextScale",
      "CODE_SIGNING_ALLOWED=YES",
      "CODE_SIGN_IDENTITY=-",
    ],
    { stdio: "inherit" },
  );
  const code = await new Promise((resolve) => command.on("exit", resolve));
  if (code !== 0) process.exitCode = code || 1;
} finally {
  await exec("xcrun", ["simctl", "ui", device, "content_size", "large"]).catch(
    () => {},
  );
  await new Promise((resolve) => server.close(resolve));
}
