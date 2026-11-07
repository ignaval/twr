// Focused typography QA on a disposable/read-only emulator clone.
import { _android, expect } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { resolve } from "node:path";
import assert from "node:assert/strict";
const adb =
  process.env.TWR_ADB ||
  `${process.env.HOME}/Library/Android/sdk/platform-tools/adb`;
const adbPort = process.env.TWR_ADB_PORT || "5040";
const adbArgs = ["-P", adbPort];
const requestedSerial = process.env.TWR_ANDROID_SERIAL || "emulator-5556";
const emulatorPort = requestedSerial.match(/^emulator-(\d+)$/)?.[1];
const serial = emulatorPort
  ? `127.0.0.1:${Number(emulatorPort) + 1}`
  : requestedSerial;
const run = (...args) =>
  execFileSync(adb, [...adbArgs, "-s", serial, ...args], {
    encoding: "utf8",
    timeout: 60000,
  });
execFileSync(adb, [...adbArgs, "start-server"]);
if (serial.includes(":")) execFileSync(adb, [...adbArgs, "connect", serial]);
run("wait-for-device");
for (let attempt = 0; attempt < 120; attempt++) {
  try {
    if (run("shell", "getprop", "sys.boot_completed").trim() === "1") break;
  } catch {
    /* adbd can restart during a cold boot. */
  }
  await new Promise((resolve) => setTimeout(resolve, 500));
}
run(
  "install",
  "-r",
  resolve("mobile/android/app/build/outputs/apk/debug/app-debug.apk"),
);
run("shell", "settings", "put", "system", "font_scale", "1");
run("shell", "am", "force-stop", "tools.twr.mobile.dev");
run("shell", "am", "start", "-n", "tools.twr.mobile.dev/.MainActivity");
let device;
let devices = [];
try {
  devices = await _android.devices({
    host: "127.0.0.1",
    port: Number(adbPort),
    omitDriverInstall: true,
  });
  device = devices.find((value) => value.serial() === serial);
  assert(device, `No QA emulator ${serial}`);
  const webview = await device.webView({ pkg: "tools.twr.mobile.dev" });
  const page = await webview.page();
  await page.locator(".mobile-onboarding").waitFor();
  await page.evaluate(() => {
    window.fontEpoch = Math.random();
    const text = document.createElement("span");
    text.id = "font-qa-fixed";
    text.textContent = "MMMMMMMMMM";
    text.style.cssText =
      "position:fixed;left:-9999px;top:0;font:20px monospace;white-space:nowrap";
    document.body.append(text);
  });
  const epoch = await page.evaluate(() => window.fontEpoch);
  const fixed = await page
    .locator("#font-qa-fixed")
    .evaluate((node) => node.getBoundingClientRect().width);
  const reports = [];
  for (const scale of [1, 1.3, 2]) {
    run("shell", "settings", "put", "system", "font_scale", String(scale));
    await expect
      .poll(
        async () =>
          page.evaluate(async () => {
            const native =
              await window.Capacitor.Plugins.TwrOffline.textScale();
            return {
              native: Math.round(native.scale * 100),
              css: Math.round(
                parseFloat(
                  getComputedStyle(document.documentElement).getPropertyValue(
                    "--text-size",
                  ),
                ) * 100,
              ),
            };
          }),
        { timeout: 15000 },
      )
      .toEqual({
        native: Math.round(scale * 100),
        css: Math.round(13 * scale * 100),
      });
    assert.equal(
      await page.evaluate(() => window.fontEpoch),
      epoch,
      "System font changes must not recreate the renderer",
    );
    const width = await page
      .locator("#font-qa-fixed")
      .evaluate((node) => node.getBoundingClientRect().width);
    assert(
      Math.abs(width - fixed) < 0.5,
      "WebView textZoom must stay neutral so CSS applies system scaling exactly once",
    );
    await page.getByRole("button", { name: "Pair a hub", exact: true }).click();
    const bad = await page.locator(".pair-form").evaluate((form) =>
      [...form.querySelectorAll("button,input")].flatMap((node) => {
        const box = node.getBoundingClientRect();
        return box.width < 43.5 ||
          box.height < 43.5 ||
          box.left < -0.5 ||
          box.right > innerWidth + 0.5
          ? [{ label: node.textContent || node.name, box: box.toJSON() }]
          : [];
      }),
    );
    reports.push({
      scale,
      native: await page.evaluate(
        async () => await window.Capacitor.Plugins.TwrOffline.textScale(),
      ),
      css: await page.evaluate(
        () => getComputedStyle(document.documentElement).fontSize,
      ),
      fixedWidth: width,
      viewport: await page.evaluate(() => ({
        width: innerWidth,
        height: innerHeight,
        scroll: document.body.scrollWidth,
      })),
      bad,
    });
    assert.deepEqual(
      bad,
      [],
      "Pairing controls must fit without clipping and keep 44-point touch targets",
    );
    if (process.env.TWR_FONT_SCREENSHOT_DIR)
      await page.screenshot({
        path: `${process.env.TWR_FONT_SCREENSHOT_DIR}/${serial}-${scale}.png`,
      });
    await page.getByRole("button", { name: "Cancel", exact: true }).click();
  }
  console.log("ANDROID_FONT_QA", JSON.stringify({ serial, reports }));
} finally {
  try {
    run("shell", "settings", "put", "system", "font_scale", "1");
  } catch {}
  await Promise.all(devices.map((value) => value.close().catch(() => {})));
}
