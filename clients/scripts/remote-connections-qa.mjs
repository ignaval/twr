// Renderer contract smoke. Native security is exercised in desktop-remote.test.ts.
import { chromium, expect } from "@playwright/test";
import { createServer } from "node:http";
import { readFileSync } from "node:fs";
import { resolve, extname } from "node:path";
import assert from "node:assert/strict";
const dist = resolve("dist");
const server = createServer((request, response) => {
  try {
    const file = resolve(
      dist,
      `.${new URL(request.url, "http://localhost").pathname}`,
    );
    if (!file.startsWith(`${dist}/`)) throw new Error("Invalid asset");
    response.setHeader(
      "Content-Type",
      { ".html": "text/html", ".js": "text/javascript", ".css": "text/css" }[
        extname(file)
      ] || "application/octet-stream",
    );
    response.end(readFileSync(file));
  } catch {
    response.statusCode = 404;
    response.end();
  }
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
let browser;
try {
  browser = await chromium.launch({
    executablePath: process.env.TWR_CHROMIUM_PATH,
  });
  const page = await browser.newPage({
    viewport: { width: 1360, height: 900 },
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript(() => {
    const boot = {
      workspace: "default",
      hostId: "local",
      platform: "darwin",
      home: "/tmp",
      version: "0.3.0",
      theme: null,
      terminals: [],
    };
    const hub = {
      id: "11111111-1111-4111-8111-111111111111",
      name: "Saved workstation",
      url: "https://remote.example:47321",
      scopes: ["read"],
      gateway_id: "gateway-test",
    };
    const listeners = new Set();
    const vault = new Map();
    const rpc = async (method) =>
      method === "hub.info"
        ? { workspace: "default", hostname: "local", version: "0.3.0" }
        : method === "runtime.status"
          ? []
          : method === "desktop.subscribe"
            ? {
                threads: [],
                attention: [],
                decisions: [],
                decisions_seen: 0,
                head: 0,
              }
            : {};
    window.twrLocal = {
      bootstrap: async () => boot,
      connect: async () => ({ workspace: "default" }),
      rpc,
      cli: async () => ({
        stdout: '[{"name":"default"}]',
        stderr: "",
        code: 0,
      }),
      catalog: async () => [],
      onEvent: () => () => {},
      configRead: async () => ({
        content: "",
        hash: "a".repeat(64),
        path: "/tmp/config",
        settings: {},
      }),
      terminalFocus: () => {},
      terminalAck: () => {},
      terminalWrite: () => {},
      terminalResize: () => {},
      pickDirectory: async () => null,
      quit: async () => {},
      copyText: async () => {},
      openExternal: async () => {},
      reveal: async () => {},
      exportText: async () => {},
      setAppearance: async () => {},
      secureRead: async (key) => vault.get(key) || null,
      secureWrite: async (key, value) => vault.set(key, value),
      secureDelete: async (key) => vault.delete(key),
    };
    window.twrRemote = {
      settings: async () => null,
      gatewayStatus: async () => ({
        running: true,
        runningVersion: "0.2.0",
        listen: "0.0.0.0:47321",
        restartRequired: !window.fixtureRestarted,
      }),
      cancelInvitation: async () => {
        window.fixtureCancelled = true;
      },
      listHubs: async () => ({ hubs: [hub] }),
      deactivateLocal: async () => {},
      restart: async () => {
        window.fixtureRestarted = true;
        return {
          url: "https://host.example:47321",
          gatewayId: "local-gateway",
          tls: true,
        };
      },
      setup: async () => ({
        url: "https://host.example:47321",
        gatewayId: "local-gateway",
        tls: false,
        restartRequired: true,
        runningVersion: "0.2.0",
        advisory: "This fixture gateway needs an explicit restart.",
      }),
      invite: async ({ scopes }) => {
        window.fixtureInvitationScope = scopes;
        return {
          url: "https://host.example:47321",
          code: "ABCD234567",
          expiresAt: Date.now() + 300000,
        };
      },
      pair: async () => hub,
      removeHub: async () => {},
      connect: async () => {
        for (const listener of listeners)
          listener({ message: { type: "authenticated", framing: true } });
      },
      disconnect: async () => {},
      addListener: async (_event, listener) => {
        listeners.add(listener);
        return { remove: async () => listeners.delete(listener) };
      },
      send: async ({ message }) => {
        const call = JSON.parse(message);
        let result = {};
        if (call.method === "bootstrap")
          result = {
            ...boot,
            hostId: hub.id,
            scopes: hub.scopes,
            allowed_methods: ["hub.info", "thread.list", "attention.list"],
            read_methods: ["hub.info", "thread.list", "attention.list"],
          };
        if (call.method === "cli")
          result = { stdout: '[{"name":"default"}]', stderr: "", code: 0 };
        if (call.method === "rpc") result = await rpc(call.params.method);
        for (const listener of listeners)
          listener({ message: { id: call.id, result } });
      },
    };
    window.fixtureListeners = listeners;
  });
  await page.goto(`http://127.0.0.1:${server.address().port}/index.html`);
  await page
    .getByRole("button", { name: "Hubs and remote access", exact: true })
    .click();
  await page.getByRole("tab", { name: "Share this computer" }).click();
  await page
    .getByLabel("Public HTTPS address")
    .fill("https://host.example:47321");
  await page.getByLabel("TLS certificate path").fill("/tmp/fullchain.pem");
  await page.getByLabel("TLS private key path").fill("/tmp/private.pem");
  await page.getByLabel("Device access").selectOption("read");
  await page
    .getByRole("button", { name: "Start gateway and check connection" })
    .click();
  const restart = page.getByRole("button", {
    name: "Restart gateway with these settings",
  });
  await restart.waitFor();
  assert.equal(
    await page
      .getByRole("button", { name: "Create one-use invitation" })
      .count(),
    0,
  );
  await restart.click();
  assert.equal(await page.evaluate(() => window.fixtureRestarted), undefined);
  await page
    .getByRole("button", {
      name: "Confirm restart · close gateway terminal sessions",
    })
    .click();
  assert.equal(await page.evaluate(() => window.fixtureRestarted), true);
  await page.getByLabel("Device access").selectOption("terminal");
  assert.equal(
    await page
      .getByRole("button", { name: "Create one-use invitation" })
      .isVisible(),
    true,
    "Scope changes must preserve the successful TLS check",
  );
  await page.getByLabel("Device access").selectOption("read");
  await page.getByRole("button", { name: "Create one-use invitation" }).click();
  await page
    .getByRole("img", {
      name: "Scan this one-use twr pairing invitation with your phone",
    })
    .waitFor();
  await page.getByText("ABCD234567", { exact: true }).waitFor();
  assert.equal(
    await page.evaluate(() => window.fixtureInvitationScope),
    "read",
  );
  await expect
    .poll(() =>
      page
        .getByRole("img", {
          name: "Scan this one-use twr pairing invitation with your phone",
        })
        .evaluate((image) => {
          const box = image.getBoundingClientRect();
          const dialog = image.closest(".dialog").getBoundingClientRect();
          return (
            box.top >= dialog.top &&
            box.bottom <= dialog.bottom &&
            box.left >= dialog.left &&
            box.right <= dialog.right
          );
        }),
    )
    .toBe(true);
  if (process.env.TWR_CONNECTIONS_SCREENSHOT)
    await page.screenshot({ path: process.env.TWR_CONNECTIONS_SCREENSHOT });
  await page.getByRole("tab", { name: "Saved hubs" }).click();
  await page
    .getByRole("article")
    .filter({ hasText: "Saved workstation" })
    .getByRole("button", { name: "Connect", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Hubs and remote access", exact: true })
    .filter({ hasText: "Saved workstation" })
    .waitFor();
  assert.equal(await page.evaluate(() => window.fixtureListeners.size), 1);
  await page.evaluate(() => {
    window.fixtureOldRemote = window.twr;
  });
  await page
    .getByRole("button", { name: "Hubs and remote access", exact: true })
    .click();
  await page
    .getByRole("button", { name: "This computer", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Hubs and remote access", exact: true })
    .filter({ hasText: "This computer" })
    .waitFor();
  assert.equal(await page.evaluate(() => window.fixtureListeners.size), 0);
  assert.match(
    await page.evaluate(async () => {
      try {
        await window.fixtureOldRemote.rpc("hub.info");
        return "accepted";
      } catch (error) {
        return error.message;
      }
    }),
    /closed/,
  );
  assert.deepEqual(errors, []);
  console.log(
    "Connection UI passed: expiring QR, read-only hub selection, local return, listener cleanup, stale API fencing.",
  );
} finally {
  await browser?.close();
  await new Promise((resolve) => server.close(resolve));
}
