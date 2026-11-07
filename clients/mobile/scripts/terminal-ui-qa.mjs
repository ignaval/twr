import { expect } from "@playwright/test";
import assert from "node:assert/strict";
import { shellAction } from "./native-actions.mjs";
export async function terminalUIQA(page) {
  await (await shellAction(page)).click();
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
  for (const [mode, expected] of [
    ["h", "1b 4f 41"],
    ["l", "1b 5b 41"],
  ]) {
    const command = `stty raw -echo; printf '\\033[?1${mode}\\r\\nCURSOR_READY_${mode}\\r\\n'; dd bs=1 count=3 2>/dev/null | od -An -tx1; printf '\\033[?1l'; stty sane\r`;
    await page.evaluate(
      ({ id, command }) => window.twr.terminalWrite(id, command),
      { id, command },
    );
    await expect
      .poll(
        () =>
          page
            .evaluate((id) => window.twr.terminalRead(id), id)
            .then((r) => r.data),
        { timeout: 15000 },
      )
      .toContain(`CURSOR_READY_${mode}\r\n`);
    await page.waitForTimeout(100);
    await session.getByRole("button", { name: "↑", exact: true }).click();
    await expect
      .poll(
        () =>
          page
            .evaluate((id) => window.twr.terminalRead(id), id)
            .then((r) => r.data.replace(/\s+/g, " ")),
        { timeout: 15000 },
      )
      .toContain(expected);
  }
  await page
    .locator('.terminal-tab.active button[aria-label^="Close "]')
    .click();
  await page
    .getByRole("dialog", { name: "Close terminal?", exact: true })
    .getByRole("button", { name: "Continue", exact: true })
    .click();
  await expect(page.locator(".terminal-session")).toHaveCount(0);
  console.log(
    "PASS: mobile embedded terminal UI, keyboard input, Control key, application/normal cursor modes and visible accessory controls",
  );
}
