import { expect } from "@playwright/test";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
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
  const checks = [];
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
    await page.mouse.move(0, 0);
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
  const visibleDialogError = async (dialog, name) => {
    const error = dialog.locator(".error-banner").first();
    await error.waitFor();
    const measure = () =>
      error.evaluate((element) => {
        const box = element.getBoundingClientRect();
        const container = element.closest(".dialog").getBoundingClientRect();
        const center = { x: box.x + box.width / 2, y: box.y + box.height / 2 };
        const hit = document.elementFromPoint(center.x, center.y);
        return {
          inside:
            box.width > 0 &&
            box.height > 0 &&
            box.left >= container.left &&
            box.right <= container.right + 1 &&
            box.top >= container.top &&
            box.bottom <= container.bottom + 1 &&
            box.top >= 0 &&
            box.bottom <= innerHeight,
          unobscured: !!hit && element.contains(hit),
          box: box.toJSON(),
          dialog: container.toJSON(),
        };
      });
    await expect
      .poll(
        async () => {
          const result = await measure();
          return result.inside && result.unobscured;
        },
        {
          message: name + " must remain visible inside the dialog",
          timeout: 5000,
        },
      )
      .toBe(true);
    checks.push({ name, ...(await measure()) });
  };
  const palette = async (appearance) => {
    await page
      .getByRole("button", { name: "Open command palette", exact: true })
      .click();
    await page
      .getByRole("textbox", { name: "Search commands", exact: true })
      .fill("thread");
    await snap(`${appearance}-commands`);
    await closeDialog();
  };
  const newThread = async (appearance, captureError = false) => {
    await page
      .getByRole("button", { name: "New thread", exact: true })
      .first()
      .click();
    const create = page.getByRole("dialog", {
      name: "New thread",
      exact: true,
    });
    await create.locator("#field-project").fill("demo");
    await create.locator("#field-name").fill("result-filters");
    await snap(`${appearance}-new-thread`);
    if (captureError) {
      await create
        .locator("#field-project")
        .fill(join(boot.home, "missing-project"));
      await create.locator('button[type="submit"]').click();
      await visibleDialogError(create, `${appearance}-new-thread-error`);
      await snap(`${appearance}-new-thread-error`);
    }
    await closeDialog();
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
  await palette("dark");
  await newThread("dark", true);
  await nav("Settings");
  await page
    .getByRole("heading", { name: "Appearance", exact: true })
    .waitFor();
  await snap("dark-settings");
  await theme("Light");
  await conversation();
  await snap("light-conversation");
  await primary(/^Thread grid/);
  await page
    .locator(".thread-grid .thread-card")
    .filter({ hasText: "search-api" })
    .click();
  await snap("light-inspector");
  await page
    .getByRole("button", { name: "Close thread details", exact: true })
    .click();
  await primary(/^Needs you/);
  await page
    .locator(".manager .attention-card")
    .filter({ hasText: "npm run test:integration" })
    .waitFor();
  await snap("light-permission");
  await conversation();
  await palette("light");
  await newThread("light", true);
  await theme("Dark");
  await conversation();

  // Trigger a genuine safe validation error; an empty shell command never executes.
  await page.setViewportSize({ width: 1000, height: 900 });
  const composer = page.getByRole("textbox", {
    name: "Message orchestrator",
    exact: true,
  });
  await composer.fill("!");
  await page.getByRole("button", { name: "Send message", exact: true }).click();
  const toast = page.locator('.toast[role="alert"]');
  await toast.waitFor();
  await expect(toast).toContainText("Enter a shell command");
  const toastGeometry = await toast.evaluate((element) => {
    const box = element.getBoundingClientRect();
    const composer = document
      .querySelector(".composer")
      .getBoundingClientRect();
    const send = document
      .querySelector('[aria-label="Send message"]')
      .getBoundingClientRect();
    const intersects = (a, b) =>
      a.left < b.right &&
      a.right > b.left &&
      a.top < b.bottom &&
      a.bottom > b.top;
    const hit = document.elementFromPoint(
      box.x + box.width / 2,
      box.y + box.height / 2,
    );
    return {
      inside:
        box.left >= 0 &&
        box.top >= 0 &&
        box.right <= innerWidth &&
        box.bottom <= innerHeight,
      unobscured: !!hit && element.contains(hit),
      overlapsComposer: intersects(box, composer),
      overlapsSend: intersects(box, send),
      box: box.toJSON(),
      composer: composer.toJSON(),
      send: send.toJSON(),
    };
  });
  expect(toastGeometry.inside).toBe(true);
  expect(toastGeometry.unobscured).toBe(true);
  expect(toastGeometry.overlapsComposer).toBe(false);
  expect(toastGeometry.overlapsSend).toBe(false);
  checks.push({ name: "dark-toast-1000", ...toastGeometry });
  await snap("dark-toast-1000");
  await page
    .getByRole("button", { name: "Dismiss error", exact: true })
    .click();
  await page.setViewportSize({ width: 1360, height: 900 });
  await conversation();

  // Long text stays in real transcript events; disclosure controls change the actual UI.
  const input = JSON.stringify({
    file: "src/search.test.ts",
    checks: Array.from(
      { length: 32 },
      (_, n) =>
        `Case ${n + 1}: pagination preserves stable ordering, empty results, and keyboard focus after navigation.`,
    ),
  });
  await rpc("thread.send", {
    thread: "release-notes",
    prompt:
      "## Release verification checklist\n\n" +
      Array.from(
        { length: 24 },
        (_, n) =>
          `- Check ${n + 1}: Verify pagination, accessible result counts, and a stable empty state.`,
      ).join("\n") +
      "\n!tool Read " +
      input +
      "\nReview checklist ready.",
  });
  await primary(/^Thread grid/);
  await page
    .locator(".thread-grid .thread-card")
    .filter({ hasText: "release-notes" })
    .click();
  await page
    .getByRole("button", { name: "Talk to this thread", exact: true })
    .click();
  await page
    .locator(".block-assistant")
    .getByText("Review checklist ready.", { exact: true })
    .waitFor();
  const user = page
    .locator(".block-user")
    .filter({ hasText: "Release verification checklist" });
  await expect(user.locator(".message-expand")).toHaveAttribute(
    "aria-expanded",
    "false",
  );
  await user.evaluate((element) => element.scrollIntoView({ block: "start" }));
  const collapsedUserHeight = await user
    .locator(".message-body")
    .evaluate((element) => element.getBoundingClientRect().height);
  await snap("dark-long-message-collapsed");
  await user.locator(".message-expand").click();
  await expect(user.locator(".message-expand")).toHaveAttribute(
    "aria-expanded",
    "true",
  );
  const expandedUserHeight = await user
    .locator(".message-body")
    .evaluate((element) => element.getBoundingClientRect().height);
  expect(expandedUserHeight).toBeGreaterThan(collapsedUserHeight + 20);
  await user.evaluate((element) => element.scrollIntoView({ block: "start" }));
  await snap("dark-long-message-expanded");
  await user.locator(".message-expand").click();
  const showTools = page.getByRole("button", {
    name: "Toggle tool calls",
    exact: true,
  });
  if (!(await showTools.getAttribute("class")).split(/\s+/).includes("on"))
    await showTools.click();
  const tool = page
    .locator(".block-tool")
    .filter({ hasText: "src/search.test.ts" });
  await tool.waitFor();
  await tool.evaluate((element) => element.scrollIntoView({ block: "start" }));
  await snap("dark-long-tool-collapsed");
  await tool.locator("details > summary").first().click();
  const toolExpand = tool.locator(".message-expand");
  await expect(toolExpand).toHaveAttribute("aria-expanded", "false");
  const collapsedToolHeight = await tool
    .locator(".message-body")
    .evaluate((element) => element.getBoundingClientRect().height);
  await tool.evaluate((element) => element.scrollIntoView({ block: "start" }));
  await snap("dark-long-tool-open-clamped");
  await toolExpand.click();
  await expect(toolExpand).toHaveAttribute("aria-expanded", "true");
  const expandedToolHeight = await tool
    .locator(".message-body")
    .evaluate((element) => element.getBoundingClientRect().height);
  expect(expandedToolHeight).toBeGreaterThan(collapsedToolHeight + 20);
  await tool.evaluate((element) => element.scrollIntoView({ block: "start" }));
  await snap("dark-long-tool-expanded");
  checks.push({
    name: "long-content-disclosures",
    collapsedUserHeight,
    expandedUserHeight,
    collapsedToolHeight,
    expandedToolHeight,
  });
  await conversation();

  // Native popup menus are outside page.screenshot. Record read-only source/OS evidence.
  let systemAppearance = "light";
  try {
    if (
      execFileSync("/usr/bin/defaults", ["read", "-g", "AppleInterfaceStyle"], {
        encoding: "utf8",
        stdio: ["ignore", "pipe", "ignore"],
      }).trim() === "Dark"
    )
      systemAppearance = "dark";
  } catch {}
  const nativeControls = await page
    .locator("#workspace-select")
    .evaluate((element) => {
      const root = getComputedStyle(document.documentElement),
        style = getComputedStyle(element);
      return {
        appAppearance: document.documentElement.dataset.theme,
        webColorScheme: root.colorScheme,
        selectColorScheme: style.colorScheme,
        selectColor: style.color,
        selectBackground: style.backgroundColor,
      };
    });
  const nativeThemeSource = [
    "desktop/main.ts",
    "desktop/preload.ts",
    "shared/appearance.ts",
  ].map((path) => {
    const lines = readFileSync(path, "utf8").split("\n");
    return {
      path,
      lines: lines
        .flatMap((text, index) =>
          /nativeTheme|themeSource|setAppearance|twr:appearance|twr:app-appearance/.test(
            text,
          )
            ? [{ line: index + 1, text: text.trim() }]
            : [],
        )
        .slice(0, 30),
    };
  });
  checks.push({
    name: "native-controls-appearance",
    systemAppearance,
    ...nativeControls,
    sourceEvidence: nativeThemeSource,
    note: "OS popup surfaces are not included by web-content screenshots; system appearance was read without modification.",
  });
  writeFileSync(
    join(directory, "mac-metrics.json"),
    JSON.stringify(
      {
        date: new Date().toISOString(),
        fixture:
          "Real packaged Mac application, disposable Rust hub, deterministic fake runtime",
        captures,
        checks,
      },
      null,
      2,
    ) + "\n",
  );
  console.log(`PASS: ${captures.length} real-app UI captures in ${directory}`);
}
