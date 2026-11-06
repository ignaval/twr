import { expect } from "@playwright/test";
import { mkdirSync, writeFileSync } from "node:fs";

/** Capture actual native WebView UI on the disposable, already paired Android QA fixture. */
export async function captureMobileUI(page, { label, thread }) {
  const prefix = label.startsWith("android-") ? label : `android-${label}`;
  if (!/^android-(phone|fold)$/.test(prefix) || !thread)
    throw new Error(
      "A phone/fold QA label and existing fixture thread are required",
    );
  expect(await page.evaluate(() => window.Capacitor.getPlatform())).toBe(
    "android",
  );
  const directory = "test-results/ui";
  mkdirSync(directory, { recursive: true });
  const captures = [],
    checks = [];
  const input = () =>
    page.getByRole("textbox", {
      name: `Message ${thread} directly`,
      exact: true,
    });
  const snap = async (name) => {
    await page.mouse.move(0, 0);
    await page.evaluate(async () => {
      await document.fonts.ready;
      await new Promise(requestAnimationFrame);
      await new Promise(requestAnimationFrame);
    });
    const path = `${directory}/${prefix}-${name}.png`;
    await page.screenshot({ path, animations: "disabled" });
    captures.push({
      name,
      path,
      ...(await page.evaluate(() => ({
        theme: document.documentElement.dataset.theme,
        keyboard: document.documentElement.dataset.keyboard,
        viewport: { width: innerWidth, height: innerHeight },
        fold: document.documentElement.dataset.fold,
      }))),
    });
  };
  const dismissKeyboard = async () => {
    const dismiss = page.getByRole("button", {
      name: "Dismiss keyboard",
      exact: true,
    });
    if (await dismiss.isVisible()) await dismiss.click();
    else
      await page.evaluate(async () => {
        document.activeElement?.blur();
        await window.Capacitor.Plugins.Keyboard.hide();
      });
    await expect
      .poll(
        () => page.evaluate(() => document.documentElement.dataset.keyboard),
        { timeout: 10000 },
      )
      .toBe("false");
    await page.waitForTimeout(350);
  };
  const openDrawer = async () => {
    if (await page.locator(".sidebar").isVisible()) return;
    const more = page
      .locator(".mobile-tabs")
      .getByRole("button", { name: "More", exact: true });
    const show = page.getByRole("button", {
      name: "Show sidebar",
      exact: true,
    });
    if (await more.isVisible()) await more.click();
    else if (await show.isVisible()) await show.click();
    else {
      await page.locator(".conversation-menu > summary").click();
      await page
        .getByRole("button", { name: "Threads and navigation", exact: true })
        .click();
    }
    await expect(page.locator(".sidebar")).toBeVisible();
  };
  const returnThread = async () => {
    await dismissKeyboard();
    const tab = page
      .locator(".mobile-tabs")
      .getByRole("button", { name: "Threads", exact: true });
    if (await tab.isVisible()) await tab.click();
    else {
      await openDrawer();
      await page
        .locator(".primary-nav")
        .getByRole("button", { name: /^Thread grid/ })
        .click();
    }
    await page
      .locator(".thread-grid .thread-card")
      .filter({ hasText: thread })
      .click();
    await page
      .getByRole("button", { name: "Talk to this thread", exact: true })
      .click();
    await input().waitFor();
    await input().fill("");
    await dismissKeyboard();
  };
  const theme = async (name) => {
    await dismissKeyboard();
    await openDrawer();
    await page
      .locator(".sidebar-footer")
      .getByRole("button", { name: "Settings", exact: true })
      .click();
    await page
      .getByRole("heading", { name: "Appearance", exact: true })
      .waitFor();
    await page.getByRole("button", { name, exact: true }).click();
    await expect(page.locator("html")).toHaveAttribute(
      "data-theme",
      name.toLowerCase(),
    );
    await returnThread();
  };
  const targets = async (locator, name) => {
    const geometry = await locator.evaluateAll((elements) =>
      elements.map((element) => {
        const box = element.getBoundingClientRect(),
          hit = document.elementFromPoint(
            box.x + box.width / 2,
            box.y + box.height / 2,
          );
        return {
          label:
            element.getAttribute("aria-label") || element.textContent.trim(),
          width: box.width,
          height: box.height,
          hit: !!hit && element.contains(hit),
        };
      }),
    );
    expect(
      geometry.length,
      name + " must expose visible controls",
    ).toBeGreaterThan(0);
    for (const target of geometry) {
      expect(target.width, JSON.stringify(target)).toBeGreaterThanOrEqual(43.9);
      expect(target.height, JSON.stringify(target)).toBeGreaterThanOrEqual(
        43.9,
      );
      expect(target.hit, JSON.stringify(target)).toBe(true);
    }
    checks.push({ name, geometry });
  };

  await theme("Dark");
  const mainNavigation = page.locator(
    ".mobile-tabs:visible button, .sidebar:visible .primary-nav > button",
  );
  await targets(mainNavigation, "main-navigation");
  const hub = page.getByRole("button", { name: /^Remote hub settings:/ });
  const hubGeometry = await hub.evaluate((element) => {
    const box = element.getBoundingClientRect(),
      name = element.querySelector(".mobile-hub-name"),
      text = name.getBoundingClientRect(),
      style = getComputedStyle(name);
    return {
      inside: box.left >= 0 && box.right <= innerWidth + 1,
      nameInside: text.left >= box.left && text.right <= box.right + 1,
      truncated: name.scrollWidth > name.clientWidth,
      overflow: style.overflow,
      textOverflow: style.textOverflow,
      whiteSpace: style.whiteSpace,
      width: box.width,
      height: box.height,
    };
  });
  expect(hubGeometry.inside).toBe(true);
  expect(hubGeometry.nameInside).toBe(true);
  if (hubGeometry.truncated) {
    expect(hubGeometry.overflow).toBe("hidden");
    expect(hubGeometry.textOverflow).toBe("ellipsis");
    expect(hubGeometry.whiteSpace).toBe("nowrap");
  }
  checks.push({ name: "hub-title", ...hubGeometry });
  await snap("dark-chat");
  await openDrawer();
  const phoneDrawer = await page.locator(".mobile-tabs").isVisible();
  if (phoneDrawer) {
    const drawer = await page.evaluate(() => ({
      footer: document.querySelector(".sidebar-footer").getBoundingClientRect()
        .bottom,
      tabs: document.querySelector(".mobile-tabs").getBoundingClientRect().top,
    }));
    expect(drawer.footer).toBeLessThanOrEqual(drawer.tabs + 1);
    checks.push({ name: "drawer-footer", ...drawer });
    await snap("dark-drawer");
  } else {
    const manage = page.locator(".manage-nav");
    if ((await manage.getAttribute("open")) === null)
      await manage.locator("summary").click();
    await snap("dark-manage");
    if ((await manage.getAttribute("open")) !== null)
      await manage.locator("summary").click();
  }
  const backdrop = page.getByRole("button", {
    name: "Close navigation",
    exact: true,
  });
  if (await backdrop.isVisible())
    await page
      .locator(".sidebar")
      .getByRole("button", { name: "Hide sidebar", exact: true })
      .click();
  await hub.click();
  const sheet = page
    .getByRole("dialog")
    .filter({ has: page.locator(".mobile-hub-sheet") });
  await sheet.waitFor();
  await snap("dark-hub-settings");
  await sheet.getByRole("button", { name: "Done", exact: true }).click();
  await theme("Light");
  await snap("light-chat");
  const menu = page.locator(".conversation-menu");
  await targets(menu.locator(":scope > summary"), "thread-more-trigger");
  await menu.locator(":scope > summary").click();
  await targets(menu.locator("button"), "thread-more-actions");
  await snap("light-thread-menu");
  await menu.locator(":scope > summary").click();

  // Empty-shell validation exercises the app's real error path without executing a command.
  await input().fill("!");
  await page.evaluate(() => window.Capacitor.Plugins.Keyboard.show());
  await expect
    .poll(
      () => page.evaluate(() => document.documentElement.dataset.keyboard),
      { timeout: 10000 },
    )
    .toBe("true");
  await page.waitForTimeout(350);
  await page.getByRole("button", { name: "Send message", exact: true }).click();
  const toast = page.locator('.toast[role="alert"]');
  await expect(toast).toContainText("Enter a shell command");
  await expect(page.locator("html")).toHaveAttribute("data-keyboard", "true");
  const toastGeometry = await toast.evaluate((element) => {
    const box = element.getBoundingClientRect(),
      composer = document.querySelector(".composer").getBoundingClientRect(),
      send = document
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
        box.right <= innerWidth + 1 &&
        box.bottom <= (visualViewport?.height || innerHeight) + 1,
      unobscured: !!hit && element.contains(hit),
      overlapsComposer: intersects(box, composer),
      overlapsSend: intersects(box, send),
      box: box.toJSON(),
      composer: composer.toJSON(),
      send: send.toJSON(),
    };
  });
  expect(toastGeometry.inside, JSON.stringify(toastGeometry)).toBe(true);
  expect(toastGeometry.unobscured, JSON.stringify(toastGeometry)).toBe(true);
  expect(toastGeometry.overlapsComposer, JSON.stringify(toastGeometry)).toBe(
    false,
  );
  expect(toastGeometry.overlapsSend, JSON.stringify(toastGeometry)).toBe(false);
  await targets(
    toast.getByRole("button", { name: "Dismiss error", exact: true }),
    "toast-dismiss",
  );
  checks.push({ name: "keyboard-toast", ...toastGeometry });
  await expect
    .poll(() =>
      page.evaluate(() => {
        const transcript = document.querySelector(".conversation-scroll");
        return (
          transcript.scrollHeight -
          transcript.scrollTop -
          transcript.clientHeight
        );
      }),
    )
    .toBeLessThanOrEqual(2);
  await snap("light-keyboard-toast");
  await toast
    .getByRole("button", { name: "Dismiss error", exact: true })
    .click();
  await input().fill("");
  await dismissKeyboard();
  await theme("Dark");
  await expect(input()).toHaveValue("");
  await expect(page.locator("html")).toHaveAttribute("data-keyboard", "false");
  writeFileSync(
    `${directory}/${prefix}-metrics.json`,
    JSON.stringify(
      {
        date: new Date().toISOString(),
        fixture: "Real Android WebView, disposable hub and fake runtime",
        captures,
        checks,
      },
      null,
      2,
    ) + "\n",
  );
  console.log(
    `PASS: ${captures.length} native ${prefix} visual captures, reachable navigation, title ellipsis, and keyboard toast`,
  );
}
