import { expect } from "@playwright/test";
import assert from "node:assert/strict";
export async function terminalUIQA(page) {
  await page.getByRole("button", { name: "Open shell", exact: true }).click();
  const session = page.locator(".terminal-session").filter({ visible: true });
  await session
    .locator(".xterm-helper-textarea")
    .waitFor({ state: "attached" });
  await session
    .locator(".xterm-helper-textarea")
    .pressSequentially("printf 'MOBILE_UI_%s\\n' PTY_OK", { delay: 2 });
  await session.locator(".xterm-helper-textarea").press("Enter");
  await page.evaluate(() => window.Capacitor.Plugins.Keyboard.show());
  await page.waitForTimeout(300);
  const id = await session.getAttribute("data-session-id");
  await expect
    .poll(
      () =>
        page
          .evaluate((id) => window.twr.terminalRead(id), id)
          .then((r) => r.data),
      { timeout: 15000 },
    )
    .toContain("MOBILE_UI_PTY_OK");
  const geometry = await session
    .locator(".terminal-accessory")
    .evaluate((node) => ({
      bottom: node.getBoundingClientRect().bottom,
      height: window.visualViewport?.height || innerHeight,
    }));
  assert(geometry.bottom <= geometry.height + 1, JSON.stringify(geometry));
  await session.getByRole("button", { name: "Ctrl", exact: true }).click();
  await expect(
    session.getByRole("button", { name: "Ctrl", exact: true }),
  ).toHaveAttribute("aria-pressed", "true");
  await session.locator(".xterm-helper-textarea").press("c");
  await expect(
    session.getByRole("button", { name: "Ctrl", exact: true }),
  ).toHaveAttribute("aria-pressed", "false");
  await page
    .locator('.terminal-tab.active button[aria-label^="Close "]')
    .click();
  await page
    .getByRole("dialog", { name: "Close terminal?", exact: true })
    .getByRole("button", { name: "Continue", exact: true })
    .click();
  await expect(page.locator(".terminal-session")).toHaveCount(0);
  console.log(
    "PASS: mobile embedded terminal UI, keyboard input, Control key and visible accessory controls",
  );
}
