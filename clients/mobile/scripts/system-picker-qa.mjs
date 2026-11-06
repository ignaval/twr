import { _android, expect } from "@playwright/test";
import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { randomUUID } from "node:crypto";

/** Exercise Android's document picker with CDP disconnected, then inspect the upload. */
export async function systemPickerQA({
  page,
  devices,
  adbRun,
  fixture,
  serial,
}) {
  const filename = `twr-system-picker-${randomUUID().slice(0, 8)}.txt`;
  const local = join(fixture, filename);
  const remote = `/sdcard/Download/${filename}`;
  writeFileSync(local, "ANDROID_SYSTEM_DOCUMENT_PICKER_OK");
  adbRun(["push", local, remote]);
  const target = await page
    .getByLabel("Upload files", { exact: true })
    .boundingBox();
  const ratio = await page.evaluate(() => devicePixelRatio);
  if (!target) throw new Error("Upload control must be visible");
  const dump = () => {
    adbRun(["shell", "rm", "-f", "/sdcard/twr-qa-ui.xml"]);
    try {
      adbRun(["shell", "uiautomator", "dump", "/sdcard/twr-qa-ui.xml"]);
    } catch (error) {
      // Some emulator images kill the dumper after successfully writing its XML.
      if (!String(error.stdout).includes("dumped to:")) throw error;
    }
    const xml = adbRun(["shell", "cat", "/sdcard/twr-qa-ui.xml"]);
    if (!xml.includes("<hierarchy")) throw new Error("Invalid native UI dump");
    return xml;
  };
  const bounds = (node) =>
    node
      .match(/bounds="\[(\d+),(\d+)\]\[(\d+),(\d+)\]"/)
      ?.slice(1)
      .map(Number);
  const tap = (node) => {
    const rect = bounds(node);
    if (!rect) throw new Error("Missing native control bounds");
    adbRun([
      "shell",
      "input",
      "tap",
      String(Math.round((rect[0] + rect[2]) / 2)),
      String(Math.round((rect[1] + rect[3]) / 2)),
    ]);
  };
  const node = (xml, label) =>
    [...xml.matchAll(/<node\b[^>]*>/g)]
      .map((m) => m[0])
      .find(
        (n) =>
          n.includes(`text="${label}"`) ||
          n.includes(`content-desc="${label}"`),
      );
  const webview = [...dump().matchAll(/<node\b[^>]*>/g)]
    .map((m) => m[0])
    .find((n) => n.includes('class="android.webkit.WebView"'));
  const frame = bounds(webview || "");
  if (!frame) throw new Error("Native WebView bounds required");
  await Promise.all(devices.map((device) => device.close()));
  adbRun([
    "shell",
    "input",
    "tap",
    String(Math.round(frame[0] + (target.x + target.width / 2) * ratio)),
    String(Math.round(frame[1] + (target.y + target.height / 2) * ratio)),
  ]);
  let selected = false;
  for (let attempt = 0; attempt < 12; attempt++) {
    const xml = dump();
    const file = node(xml, filename);
    if (file) {
      tap(file);
      selected = true;
      break;
    }
    const files = node(xml, "Files");
    if (files) {
      tap(files);
      continue;
    }
    const downloads = node(xml, "Downloads");
    if (downloads) {
      tap(downloads);
      continue;
    }
    const roots = node(xml, "Show roots");
    if (roots) {
      tap(roots);
      continue;
    }
    await new Promise((resolve) => setTimeout(resolve, 300));
  }
  if (!selected)
    throw new Error(
      "Could not select the test file through Android's system document picker",
    );
  const fresh = await _android.devices({ omitDriverInstall: true });
  const device = fresh.find((d) => d.serial() === serial);
  if (!device) throw new Error("Android device disconnected");
  const web = await device.webView({ pkg: "tools.twr.mobile.dev" });
  const next = await web.page();
  await expect
    .poll(() => next.locator(".composer textarea").inputValue(), {
      timeout: 30000,
    })
    .toContain(filename);
  console.log(
    "PASS: Android system document picker returns a real file to the app upload flow",
  );
  adbRun(["shell", "rm", remote, "/sdcard/twr-qa-ui.xml"]);
  return { page: next, device, devices: fresh };
}
