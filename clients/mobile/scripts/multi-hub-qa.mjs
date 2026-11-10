import { spawn, execFileSync } from "node:child_process";
import { stopFixture } from "../../scripts/fixture-cleanup.mjs";
import { mkdirSync, writeFileSync, existsSync } from "node:fs";
import { join } from "node:path";
import { expect } from "@playwright/test";
import assert from "node:assert/strict";
export async function multiHubQA({
  page,
  fixture,
  repo,
  env,
  bin,
  adbRun,
  originalGateway,
}) {
  const home = join(fixture, "second-hub");
  mkdirSync(home);
  writeFileSync(
    join(home, "config.toml"),
    `default_runtime="fake"\n[projects.demo]\nrepo=${JSON.stringify(repo)}\n[orchestrator]\nautostart=false\n[tui]\nsuggest=false\nsummaries=false\n`,
    { mode: 0o600 },
  );
  const remoteEnv = { ...env, HOME: home, TWR_HOME: home };
  let hub,
    workHub,
    gateway,
    out = "",
    err = "";
  async function notification(gatewayId, workspace, title) {
    await page.evaluate(
      async ({ gatewayId, workspace, title }) => {
        await window.Capacitor.Plugins.LocalNotifications.schedule({
          notifications: [
            {
              id: 925311,
              title,
              body: "Open the original hub",
              channelId: "twr-attention",
              isExactNotification: false,
              extra: { gateway_id: gatewayId, workspace },
            },
          ],
        });
      },
      { gatewayId, workspace, title },
    );
    adbRun(["shell", "cmd", "statusbar", "expand-notifications"]);
    let tapped = false;
    for (let i = 0; i < 10; i++) {
      adbRun(["shell", "uiautomator", "dump", "/sdcard/twr-qa-ui.xml"]);
      const xml = adbRun(["shell", "cat", "/sdcard/twr-qa-ui.xml"]);
      const node = [...xml.matchAll(/<node\b[^>]*>/g)]
        .map((m) => m[0])
        .find((n) => n.includes(`text="${title}"`));
      const r = node
        ?.match(/bounds="\[(\d+),(\d+)\]\[(\d+),(\d+)\]"/)
        ?.slice(1)
        .map(Number);
      if (r) {
        adbRun([
          "shell",
          "input",
          "tap",
          String(Math.round((r[0] + r[2]) / 2)),
          String(Math.round((r[1] + r[3]) / 2)),
        ]);
        tapped = true;
        break;
      }
    }
    assert(tapped, "Cross-hub notification must appear in the system shade");
  }
  try {
    hub = spawn(join(bin, "twr"), ["hub"], { env: remoteEnv, stdio: "ignore" });
    for (let i = 0; i < 100 && !existsSync(join(home, "hub.sock")); i++)
      await new Promise((r) => setTimeout(r, 100));
    gateway = spawn(
      join(bin, "twr-mobile-gateway"),
      [
        "--development",
        "--pair",
        "--allow-admin",
        "--allow-terminal",
        "--binary-dir",
        bin,
        "--listen",
        "127.0.0.1:47323",
      ],
      { env: remoteEnv, stdio: ["ignore", "pipe", "pipe"] },
    );
    gateway.stdout.on("data", (d) => (out += d));
    gateway.stderr.on("data", (d) => (err += d));
    for (let i = 0; i < 100 && !out.includes("Pairing code"); i++) {
      assert.equal(gateway.exitCode, null, err);
      await new Promise((r) => setTimeout(r, 100));
    }
    const code = out.match(/Pairing code[^:]+: (\w+)/)?.[1];
    assert(code, err);
    adbRun(["reverse", "tcp:47323", "tcp:47323"]);
    await page.evaluate(() => {
      window.__qaPreviousHubAPI = window.twr;
    });
    await page.getByRole("button", { name: /^Remote hub settings:/ }).click();
    await page.getByRole("button", { name: "Switch hub", exact: true }).click();
    await page.getByRole("button", { name: /Pair.*hub/i }).click();
    await page.getByLabel("Hub name", { exact: true }).fill("Second hub QA");
    await page
      .getByLabel("Hub URL", { exact: true })
      .fill("http://127.0.0.1:47323");
    await page.getByLabel("Pairing code", { exact: true }).fill(code);
    await page
      .getByLabel("This device", { exact: true })
      .fill("Second hub device");
    await page
      .getByRole("button", { name: "Pair and connect", exact: true })
      .click();
    await expect(
      page.getByRole("button", { name: /^Remote hub settings:/ }),
    ).toHaveText("Second hub QA");
    await expect
      .poll(() =>
        page.evaluate(async () => {
          try {
            return (await window.twr.rpc("thread.list")).length;
          } catch {
            return -1;
          }
        }),
      )
      .toBe(0);
    await page.evaluate(() =>
      window.twr.rpc("thread.new", {
        project: "demo",
        name: "second-hub-worker",
        runtime: "fake",
      }),
    );
    const late = await page.evaluate(async () => {
      try {
        await window.__qaPreviousHubAPI.rpc("thread.send", {
          thread: "second-hub-worker",
          prompt: "LATE_CALL_MUST_NOT_REACH_THIS_HUB",
        });
        return "unexpected success";
      } catch (error) {
        return String(error);
      }
    });
    assert.match(late, /closed/);
    await page
      .locator(".mobile-tabs")
      .getByRole("button", { name: "Threads", exact: true })
      .click();
    await expect(
      page
        .locator(".thread-grid .thread-card")
        .filter({ hasText: "second-hub-worker" }),
    ).toBeVisible();
    await expect(
      page
        .locator(".thread-grid .thread-card")
        .filter({ hasText: "android-worker" }),
    ).toHaveCount(0);
    execFileSync(join(bin, "twr"), ["ws", "new", "work"], {
      env: remoteEnv,
      stdio: "ignore",
    });
    workHub = spawn(join(bin, "twr"), ["-w", "work", "hub"], {
      env: remoteEnv,
      stdio: "ignore",
    });
    for (let i = 0; i < 100 && !existsSync(join(home, "ws", "work.sock")); i++)
      await new Promise((r) => setTimeout(r, 100));
    // The sidebar lists every workspace, re-read every 15 s while it shows.
    const showSidebar = page.getByRole("button", {
      name: "Show sidebar",
      exact: true,
    });
    if (await showSidebar.isVisible()) await showSidebar.click();
    const work = page.locator('.workspace-entry[data-workspace="work"]');
    await expect(work).toHaveCount(1, { timeout: 40000 });
    await work.locator(".workspace-orchestrator").click();
    await expect
      .poll(() =>
        page.evaluate(async () => {
          try {
            return (await window.twr.rpc("hub.info")).workspace;
          } catch {
            return "";
          }
        }),
      )
      .toBe("work");
    await page.evaluate(() =>
      window.twr.rpc("thread.new", {
        project: "demo",
        name: "second-work-worker",
        runtime: "fake",
      }),
    );
    const secondGateway = await page.evaluate(
      async () =>
        (await window.Capacitor.Plugins.TwrTransport.listHubs()).hubs.find(
          (h) => h.url.includes(":47323"),
        ).gateway_id,
    );
    await notification(originalGateway, "default", "twr · hub switch QA");
    await expect(
      page.getByRole("button", { name: /^Remote hub settings:/ }),
    ).toHaveText("QA workstation", { timeout: 30000 });
    await expect(
      page.locator(".mobile-tabs").getByRole("button", { name: /Needs you/ }),
    ).toHaveAttribute("aria-current", "page", { timeout: 30000 });
    await page
      .locator(".mobile-tabs")
      .getByRole("button", { name: "Threads", exact: true })
      .click();
    await expect(
      page
        .locator(".thread-grid .thread-card")
        .filter({ hasText: "android-worker" }),
    ).toBeVisible();
    await expect(
      page
        .locator(".thread-grid .thread-card")
        .filter({ hasText: "second-hub-worker" }),
    ).toHaveCount(0);
    await notification(secondGateway, "work", "twr · work QA");
    await expect(
      page.getByRole("button", { name: /^Remote hub settings:/ }),
    ).toHaveText("Second hub QA", { timeout: 30000 });
    await expect(page.locator(".workspace-breadcrumb")).toHaveText("work", {
      timeout: 30000,
    });
    await expect
      .poll(() =>
        page
          .evaluate(() => window.twr.rpc("hub.info"))
          .then((v) => v.workspace),
      )
      .toBe("work");
    await notification(originalGateway, "default", "twr · return QA");
    await expect(
      page.getByRole("button", { name: /^Remote hub settings:/ }),
    ).toHaveText("QA workstation", { timeout: 30000 });
    console.log(
      "PASS: a real notification switches between two remote hubs and binds the new UI subscriptions",
    );
    gateway.kill();
    await new Promise((resolve) => gateway.once("exit", resolve));
    await notification(secondGateway, "work", "twr · offline hub QA");
    // The previously visited hub can reopen its encrypted cache even though
    // its gateway is unavailable. Verify its identity and disabled authority.
    await expect(page.locator(".offline-banner")).toContainText("Offline", {
      timeout: 40000,
    });
    await expect(
      page.getByRole("button", { name: /^Remote hub settings:/ }),
    ).toHaveText("Second hub QA");
    await expect(
      page.getByRole("button", { name: "Search conversations", exact: true }),
    ).toBeDisabled();
    assert.equal(await page.locator(".live-mark").count(), 0);
    const dismissError = page.getByRole("button", {
      name: "Dismiss error",
      exact: true,
    });
    if (await dismissError.isVisible()) await dismissError.click();
    const dismissConnection = page.locator(".mobile-error button");
    if (await dismissConnection.isVisible()) await dismissConnection.click();
    await page.getByRole("button", { name: /^Remote hub settings:/ }).click();
    await page.getByRole("button", { name: "Switch hub", exact: true }).click();
    await expect(page.locator(".mobile-onboarding")).toBeVisible();
    await page
      .locator(".hub-connect")
      .filter({ hasText: "QA workstation" })
      .click();
    await expect(
      page.getByRole("button", { name: /^Remote hub settings:/ }),
    ).toHaveText("QA workstation");
    console.log(
      "PASS: an offline notification hub opens its own cache without live authority and switches back cleanly",
    );
  } finally {
    gateway?.kill();
    hub?.kill();
    workHub?.kill();
    stopFixture(join(bin, "twr"), remoteEnv, {
      workspaces: ["default", "work"],
    });
  }
}
