import { expect } from "@playwright/test";
import { mkdirSync, writeFileSync } from "node:fs";
import { basename, join } from "node:path";

/** Real UI captures. Only call from packaged-smoke's disposable fake-runtime fixture. */
export async function captureUI(page, { hubPids } = {}) {
  const boot = await page.evaluate(() => window.twr.bootstrap());
  if (!basename(boot.home).startsWith("twr-package-"))
    throw new Error(
      "UI captures require the disposable packaged-app QA fixture",
    );
  if (!hubPids)
    throw new Error("UI captures require the fixture's hub cleanup registry");
  const directory = "test-results/ui";
  mkdirSync(directory, { recursive: true });
  const captures = [];
  const rpc = (method, params = {}) =>
    page.evaluate(({ method, params }) => window.twr.rpc(method, params), {
      method,
      params,
    });
  const nav = async (name) => {
    const legacy = page
      .locator(".utility-nav")
      .getByRole("button", { name, exact: true });
    if (await legacy.isVisible()) return legacy.click();
    if (name === "Settings")
      return page
        .locator(".sidebar-footer")
        .getByRole("button", { name, exact: true })
        .click();
    const manage = page.locator(".manage-nav");
    if ((await manage.getAttribute("open")) === null)
      await manage.locator("summary").click();
    return manage.getByRole("button", { name, exact: true }).click();
  };
  const primary = (name) =>
    page.locator(".primary-nav").getByRole("button", { name }).click();
  const closeDialog = () =>
    page
      .getByRole("dialog")
      .getByRole("button", { name: "Close dialog", exact: true })
      .click();
  const snap = async (name) => {
    await page.evaluate(async () => {
      await document.fonts.ready;
      document.activeElement?.blur();
      await new Promise(requestAnimationFrame);
      await new Promise(requestAnimationFrame);
    });
    const path = join(directory, `mac-${name}.png`);
    await page.screenshot({ path, animations: "disabled" });
    const metrics = await page.evaluate(() => {
      const typography = {};
      for (const selector of [
        ".topbar",
        ".thread-card-title > b",
        ".thread-card-runtime",
        ".markdown",
        ".composer textarea",
        ".dialog h2",
        ".field",
      ]) {
        const element = [...document.querySelectorAll(selector)].find(
          (node) => node.getClientRects().length,
        );
        if (!element) continue;
        const style = getComputedStyle(element);
        typography[selector] = {
          font: style.fontFamily,
          size: style.fontSize,
          lineHeight: style.lineHeight,
          color: style.color,
        };
      }
      return {
        theme: document.documentElement.dataset.theme,
        viewport: { width: innerWidth, height: innerHeight },
        horizontalOverflow: document.documentElement.scrollWidth > innerWidth,
        typography,
      };
    });
    expect(metrics.horizontalOverflow, name + " must fit the window").toBe(
      false,
    );
    captures.push({ name, path, ...metrics });
  };
  const theme = async (value) => {
    await nav("Settings");
    await page
      .getByRole("heading", { name: "Appearance", exact: true })
      .waitFor();
    await page.getByRole("button", { name: value, exact: true }).click();
    await expect(page.locator("html")).toHaveAttribute(
      "data-theme",
      value.toLowerCase(),
    );
  };
  const conversation = async () => {
    await primary(/^Orchestrator/);
    const manage = page.locator(".manage-nav");
    if ((await manage.count()) && (await manage.getAttribute("open")) !== null)
      await manage.locator("summary").click();
    await page
      .locator(".block-assistant")
      .filter({ hasText: "Search is ready for review" })
      .waitFor();
    const details = page.getByRole("button", {
      name: "Close thread details",
      exact: true,
    });
    if (await details.isVisible()) await details.click();
    await page
      .getByRole("textbox", { name: "Message orchestrator", exact: true })
      .fill("Run the integration tests, then prepare a summary.");
    await page.evaluate(() => {
      const scroller = document.querySelector(".conversation-scroll");
      scroller.scrollTop = 0;
    });
  };

  // A separate real workspace keeps earlier QA history and benchmark dumps out of the shots.
  await page
    .getByRole("button", { name: "Manage workspaces", exact: true })
    .click();
  const workspace = page.getByRole("dialog", {
    name: "Workspaces",
    exact: true,
  });
  await workspace
    .getByLabel("New workspace name", { exact: true })
    .fill("product");
  await workspace
    .getByRole("button", { name: "Create workspace", exact: true })
    .click();
  await expect
    .poll(
      () => page.evaluate(async () => (await window.twr.bootstrap()).workspace),
      { timeout: 30000 },
    )
    .toBe("product");
  await page
    .locator(".connection-status")
    .filter({ hasText: "Hub connected" })
    .waitFor();
  hubPids.add((await rpc("hub.info")).pid);
  for (const name of ["search-api", "web-ui", "release-notes"])
    await rpc("thread.new", {
      project: "demo",
      name,
      runtime: "fake",
      mode: "manual",
    });
  await rpc("thread.send", {
    thread: "search-api",
    prompt:
      "Cursor pagination is complete. Empty result sets and stable ordering are covered by the tests.",
  });
  await rpc("thread.send", {
    thread: "release-notes",
    prompt:
      "The changelog is ready. It covers cursor pagination, accessibility improvements, and migration notes.",
  });
  // The fake adapter raises a real revision-bound permission without running this command.
  await rpc("thread.send", {
    thread: "web-ui",
    prompt: "!perm Bash npm run test:integration",
  });
  await expect
    .poll(
      async () =>
        (await rpc("attention.list", { all: true })).some(
          (item) => item.reason === "permission",
        ),
      { timeout: 30000 },
    )
    .toBe(true);
  await rpc("say", {
    to: "orchestrator",
    message:
      "## Search is ready for review\n\nCursor pagination and the changelog are complete. Web UI needs permission to run integration tests.\n\n```ts\nawait search({ query, after: cursor, limit: 25 });\n```\n\nAfter testing, I can prepare the review.",
  });

  await theme("Dark");
  await conversation();
  await snap("dark-conversation");
  // Responsive viewport checks run inside the real native client and Chromium engine.
  for (const width of [1000, 1440]) {
    await page.setViewportSize({ width, height: 900 });
    await conversation();
    await snap(`dark-conversation-${width}`);
  }
  await page.setViewportSize({ width: 1360, height: 900 });
  await primary(/^Thread grid/);
  await page
    .locator(".thread-grid .thread-card")
    .filter({ hasText: "search-api" })
    .waitFor();
  await snap("dark-grid");
  await page
    .locator(".thread-grid .thread-card")
    .filter({ hasText: "search-api" })
    .click();
  await snap("dark-inspector");
  await page
    .getByRole("button", { name: "Close thread details", exact: true })
    .click();
  await primary(/^Needs you/);
  await page
    .locator(".manager .attention-card")
    .filter({ hasText: "npm run test:integration" })
    .waitFor();
  await snap("dark-permission");
  await conversation();
  await page
    .getByRole("button", { name: "Open command palette", exact: true })
    .click();
  await page
    .getByRole("textbox", { name: "Search commands", exact: true })
    .fill("thread");
  await snap("dark-commands");
  await closeDialog();
  await page
    .getByRole("button", { name: "New thread", exact: true })
    .first()
    .click();
  const create = page.getByRole("dialog", { name: "New thread", exact: true });
  await create.locator("#field-project").fill("demo");
  await create.locator("#field-name").fill("result-filters");
  await snap("dark-new-thread");
  await create
    .locator("#field-project")
    .fill(join(boot.home, "missing-project"));
  await create.locator('button[type="submit"]').click();
  await create.locator(".error-banner").waitFor();
  await snap("dark-new-thread-error");
  await closeDialog();
  await nav("Settings");
  await page
    .getByRole("heading", { name: "Appearance", exact: true })
    .waitFor();
  await snap("dark-settings");
  await theme("Light");
  await conversation();
  await snap("light-conversation");
  await theme("Dark");
  await conversation();
  writeFileSync(
    join(directory, "mac-metrics.json"),
    JSON.stringify(
      {
        date: new Date().toISOString(),
        fixture:
          "Real packaged Mac application, disposable Rust hub, deterministic fake runtime",
        captures,
      },
      null,
      2,
    ) + "\n",
  );
  console.log(`PASS: ${captures.length} real-app UI captures in ${directory}`);
}
