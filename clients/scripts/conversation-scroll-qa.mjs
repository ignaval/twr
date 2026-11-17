// npm run test:conversation-scroll. Real shared UI/virtualizer; no hub or credentials.
import { chromium, expect } from "@playwright/test";
import { createServer } from "vite";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));
const server = await createServer({
  root,
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "conversation-scroll-fixture",
      configureServer(server) {
        server.middlewares.use(
          "/__conversation_scroll__",
          async (_request, response, next) => {
            try {
              response.setHeader("Content-Type", "text/html");
              response.end(
                await server.transformIndexHtml(
                  "/__conversation_scroll__",
                  `
            <html><head><meta name="viewport" content="width=device-width,initial-scale=1"></head>
            <body><div id="root"></div>
            <script type="module" src="/tests/fixtures/conversation-scroll.tsx"></script></body></html>
          `,
                ),
              );
            } catch (error) {
              next(error);
            }
          },
        );
      },
    },
  ],
});
let browser;
try {
  await server.listen();
  const address = server.httpServer.address();
  const url = `http://127.0.0.1:${address.port}/__conversation_scroll__`;
  browser = await chromium.launch({
    executablePath: process.env.TWR_CHROMIUM_PATH,
  });
  const frames = (page) =>
    page.evaluate(
      () =>
        new Promise((resolve) =>
          requestAnimationFrame(() => requestAnimationFrame(resolve)),
        ),
    );
  const pending = (page, turn) =>
    expect
      .poll(() =>
        page.evaluate(() => window.conversationScrollQA?.pending() || []),
      )
      .toContain(`orch:${turn}`);
  const release = (page, turn) =>
    page.evaluate(
      (turn) => window.conversationScrollQA.release("orch", turn),
      turn,
    );
  const readNow = (page) =>
    page.locator(".conversation-scroll").evaluate((scroll) => {
      const top = scroll.getBoundingClientRect().top;
      const message = [...scroll.querySelectorAll(".block-assistant")].find(
        (row) => {
          const rect = row.getBoundingClientRect();
          return rect.bottom > top + 1 && rect.top < top + scroll.clientHeight;
        },
      );
      if (!message) return null;
      return {
        text: message.querySelector(".markdown p").textContent,
        offset: message.getBoundingClientRect().top - top,
        index: Number(message.dataset.index),
        scrollTop: scroll.scrollTop,
        margin: scroll.querySelector(".transcript-virtual").offsetTop,
      };
    });
  const read = async (page) => {
    let position;
    await expect
      .poll(
        async () => {
          position = await readNow(page);
          return position !== null;
        },
        { message: "Wait for a visible virtual message to anchor" },
      )
      .toBe(true);
    return position;
  };
  const unchanged = async (page, saved) => {
    const position = () =>
      page.getByText(saved.text, { exact: true }).evaluate((node) => {
        const row = node.closest("article");
        return (
          row.getBoundingClientRect().top -
          row.closest(".conversation-scroll").getBoundingClientRect().top
        );
      });
    await frames(page);
    expect(
      Math.abs((await position()) - saved.offset),
      saved.text,
    ).toBeLessThanOrEqual(1);
    // Also cover delayed ResizeObserver measurements after scrolling has settled.
    await page.waitForTimeout(200);
    expect(
      Math.abs((await position()) - saved.offset),
      saved.text,
    ).toBeLessThanOrEqual(1);
  };
  const scrollTo = async (page, fraction) => {
    await page.locator(".conversation-scroll").evaluate((node, fraction) => {
      node.scrollTop = (node.scrollHeight - node.clientHeight) * fraction;
    }, fraction);
    await frames(page);
  };
  const atBottom = (page) =>
    expect
      .poll(() =>
        page
          .locator(".conversation-scroll")
          .evaluate(
            (node) => node.scrollHeight - node.scrollTop - node.clientHeight,
          ),
      )
      .toBeLessThanOrEqual(2);
  const shortcutSize = (page) =>
    page.locator(".latest-user-message button").evaluate((button) => {
      const preview = button.querySelector(".latest-user-preview");
      const style = getComputedStyle(button);
      const children = [...button.children];
      return {
        width: button.getBoundingClientRect().width,
        rowWidth: button.parentElement.getBoundingClientRect().width,
        contentWidth:
          children.reduce(
            (sum, child) => sum + child.getBoundingClientRect().width,
            0,
          ) +
          parseFloat(style.columnGap) * (children.length - 1) +
          parseFloat(style.paddingLeft) +
          parseFloat(style.paddingRight),
        previewWidth: preview.clientWidth,
        previewScrollWidth: preview.scrollWidth,
      };
    });
  const landedOnUser = async (page, text) => {
    const user = page.locator(".block-user").filter({ hasText: text });
    await expect(user).toBeFocused();
    await expect
      .poll(() =>
        user.evaluate((node) => {
          const scroll = node.closest(".conversation-scroll");
          return Math.abs(
            node.getBoundingClientRect().top -
              scroll.getBoundingClientRect().top,
          );
        }),
      )
      .toBeLessThanOrEqual(1);
    // Keep checking after the virtualizer's reconciliation and row measurement
    // frames, not just when focus first reaches the destination.
    await page.waitForTimeout(250);
    await expect(user).toBeFocused();
    expect(
      await user.evaluate((node) =>
        Math.abs(
          node.getBoundingClientRect().top -
            node.closest(".conversation-scroll").getBoundingClientRect().top,
        ),
      ),
    ).toBeLessThanOrEqual(1);
  };

  for (const mobile of [false, true]) {
    const page = await browser.newPage({
      viewport: mobile
        ? { width: 390, height: 844 }
        : { width: 1280, height: 800 },
      reducedMotion: "reduce",
    });
    const errors = [];
    page.setDefaultTimeout(5000);
    page.on("pageerror", (error) => errors.push(error.message));
    await page.addInitScript((mobile) => {
      document.addEventListener("DOMContentLoaded", () => {
        document.documentElement.dataset.mobile = String(mobile);
      });
    }, mobile);

    await page.goto(url);
    await pending(page, 3);
    await scrollTo(page, 1);
    await scrollTo(page, 1);
    await atBottom(page);
    await scrollTo(page, 0.6);
    const firstPosition = await read(page);
    await scrollTo(page, 0.35); // Move again while the same request is pending.
    const secondPosition = await read(page);
    expect(secondPosition.text).not.toBe(firstPosition.text);
    await release(page, 3);
    await unchanged(page, secondPosition);
    await pending(page, 2);
    await scrollTo(page, 0.55);
    await page.evaluate(() => window.conversationScrollQA.append());
    await page.waitForTimeout(70);
    const thirdPosition = await read(page);
    await release(page, 2);
    await unchanged(page, thirdPosition);
    await pending(page, 1);
    await scrollTo(page, 0.45);
    const finalPosition = await read(page);
    await release(page, 1);
    await unchanged(page, finalPosition);
    await expect(page.locator(".load-earlier")).toHaveCount(0);
    await expect(page.locator(".latest-user-empty")).toHaveText(
      "No messages from you yet",
    );
    console.log(
      `PASS ${mobile ? "phone" : "desktop"}: repeated background prepends preserve message and pixel offset after scrolling during pending requests and live append`,
    );

    await page.goto(url + "?with-user");
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:4",
    );
    await page.locator(".load-earlier").click();
    await pending(page, 3);
    await scrollTo(page, 0.65);
    await scrollTo(page, 0.4);
    const manualPosition = await read(page);
    await release(page, 3);
    await unchanged(page, manualPosition);
    await page.locator(".latest-user-message button").focus();
    await page.keyboard.press("Enter");
    await expect(page.locator(".block-user:focus-visible")).toContainText(
      "Task orch:4",
    );
    console.log(
      `PASS ${mobile ? "phone" : "desktop"}: manual history load preserves the position reached while waiting; latest-user keyboard jump still works`,
    );

    await page.goto(url);
    await pending(page, 3);
    await scrollTo(page, 1);
    await scrollTo(page, 1);
    await atBottom(page);
    await release(page, 3);
    await pending(page, 2);
    await atBottom(page);
    await release(page, 2);
    await pending(page, 1);
    await release(page, 1);
    await atBottom(page);
    await page.evaluate(() => window.conversationScrollQA.append());
    await expect(page.getByText(/Live reply below the viewport/)).toBeVisible();
    await atBottom(page);
    console.log(
      `PASS ${mobile ? "phone" : "desktop"}: automatic bottom-follow remains active`,
    );

    // Resolve a predecessor lookup immediately: its prepend can beat the
    // virtualizer's outstanding scrollToIndex reconciliation frames.
    await page.goto(url + "?with-user&fast-history");
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:4",
    );
    await atBottom(page);
    await expect(page.locator(".block-user")).toHaveCount(0);
    for (const turn of [4, 3, 2]) {
      await page.locator(".latest-user-message button").click();
      await landedOnUser(page, `Task orch:${turn}`);
      await expect(page.locator(".latest-user-preview")).toHaveText(
        `Task orch:${turn - 1}`,
      );
      expect(
        await page.evaluate(() =>
          window.conversationScrollQA.historyRequests(),
        ),
      ).toEqual(
        Array.from({ length: 5 - turn }, (_, index) => `orch:${3 - index}`),
      );
    }

    // Also deliver a request already in flight in the very same browser task
    // as the click, before any jump-settlement animation frame can run.
    await page.goto(url + "?with-user");
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:4",
    );
    await atBottom(page);
    await page.locator(".load-earlier").evaluate((node) => node.click());
    await pending(page, 3);
    await page.evaluate(() => {
      document.querySelector(".latest-user-message button").click();
      window.conversationScrollQA.release("orch", 3);
    });
    await landedOnUser(page, "Task orch:4");
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:3",
    );
    expect(
      await page.evaluate(() => window.conversationScrollQA.historyRequests()),
    ).toEqual(["orch:3"]);
    console.log(
      `PASS ${mobile ? "phone" : "desktop"}: fast and already-in-flight prepends during jump settlement preserve the clicked message, focus, and single-step destination`,
    );

    await page.goto(url + "?with-user");
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:4",
    );
    await atBottom(page);
    const compact = await shortcutSize(page);
    expect(Math.abs(compact.width - compact.contentWidth)).toBeLessThanOrEqual(
      1,
    );
    expect(compact.width).toBeLessThan(compact.rowWidth - 40);
    const rowBox = await page.locator(".latest-user-message").boundingBox();
    const beforeEmptyClick = await read(page);
    const emptyPoint = {
      x: rowBox.x + rowBox.width - 4,
      y: rowBox.y + rowBox.height / 2,
    };
    expect(
      await page.evaluate(
        ({ x, y }) =>
          !!document
            .elementFromPoint(x, y)
            ?.closest(".latest-user-message button"),
        emptyPoint,
      ),
    ).toBe(false);
    await page.mouse.click(emptyPoint.x, emptyPoint.y);
    await unchanged(page, beforeEmptyClick);
    await expect(page.locator(".block-user:focus")).toHaveCount(0);
    expect(
      await page.evaluate(() => window.conversationScrollQA.pending()),
    ).toEqual([]);
    await page.evaluate(() =>
      window.conversationScrollQA.appendUser("A somewhat longer request"),
    );
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "A somewhat longer request",
    );
    const wider = await shortcutSize(page);
    expect(wider.width).toBeGreaterThan(compact.width);
    expect(wider.width).toBeLessThan(wider.rowWidth);
    const longPrompt = "UnbrokenRequest".repeat(100);
    await page.evaluate(
      (prompt) => window.conversationScrollQA.appendUser(prompt),
      longPrompt,
    );
    await expect(page.locator(".latest-user-preview")).toHaveText(longPrompt);
    const long = await shortcutSize(page);
    expect(long.width).toBeLessThanOrEqual(long.rowWidth);
    expect(long.previewScrollWidth).toBeGreaterThan(long.previewWidth);
    await page.setViewportSize({
      width: mobile ? 320 : 900,
      height: mobile ? 844 : 800,
    });
    const narrow = await shortcutSize(page);
    expect(narrow.width).toBeLessThan(long.width);
    expect(narrow.width).toBeLessThanOrEqual(narrow.rowWidth);
    expect(narrow.previewScrollWidth).toBeGreaterThan(narrow.previewWidth);
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= window.innerWidth,
      ),
    ).toBe(true);
    await page.evaluate(() => window.conversationScrollQA.appendUser("Tiny"));
    await expect(page.locator(".latest-user-preview")).toHaveText("Tiny");
    expect((await shortcutSize(page)).width).toBeLessThan(compact.width);
    await page.locator(".latest-user-message button").focus();
    await page.keyboard.press("Enter");
    await expect(page.locator(".block-user:focus-visible")).toContainText(
      "Tiny",
    );
    await page.setViewportSize({
      width: mobile ? 390 : 1280,
      height: mobile ? 844 : 800,
    });
    console.log(
      `PASS ${mobile ? "phone" : "desktop"}: shortcut width follows its text, empty row space is not clickable, long text truncates within responsive bounds, and keyboard jumping remains available`,
    );

    await page.goto(url + "?with-user&combined");
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:4",
    );
    await atBottom(page);
    // The destination is outside the mounted virtual rows.
    await expect(page.locator(".block-user")).toHaveCount(0);
    await page.locator(".latest-user-message button").focus();
    await page.keyboard.press("Enter");
    await expect(page.locator(".block-user:focus-visible")).toContainText(
      "Task orch:4",
    );
    await pending(page, 3);
    const jumpPosition = await read(page);
    await page.evaluate(() => window.conversationScrollQA.appendUser());
    await unchanged(page, jumpPosition);
    await page.locator(".latest-user-message button").focus();
    await release(page, 3);
    await unchanged(page, jumpPosition);
    await expect(page.locator(".latest-user-message button")).toBeFocused();
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:3",
    );
    // Crossing within the same user row must update, in both directions.
    await page.locator(".conversation-scroll").evaluate((node) => {
      node.scrollTop += 5;
    });
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:4",
    );
    await page.locator(".conversation-scroll").evaluate((node) => {
      node.scrollTop -= 5;
    });
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:3",
    );
    for (const turn of [3, 2, 1]) {
      await page.locator(".latest-user-message button").click();
      await expect(page.locator(".block-user:focus")).toContainText(
        `Task orch:${turn}`,
      );
      if (turn > 1) {
        await pending(page, turn - 1);
        const saved = await read(page);
        await release(page, turn - 1);
        await unchanged(page, saved);
        await expect(page.locator(".latest-user-preview")).toHaveText(
          `Task orch:${turn - 1}`,
        );
      }
    }
    await expect(page.locator(".latest-user-message button")).toHaveAttribute(
      "aria-disabled",
      "true",
    );
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "No earlier messages from you",
    );
    const earliestPosition = await read(page);
    await page
      .locator(".latest-user-message button")
      .evaluate((node) => node.click());
    await unchanged(page, earliestPosition);
    // Manual scrolling changes the preview without changing keyboard focus.
    const composerFocus = page.getByRole("textbox", {
      name: "Message orchestrator",
      exact: true,
    });
    await composerFocus.focus();
    await scrollTo(page, 0.65);
    await expect(composerFocus).toBeFocused();
    const browsingPosition = await read(page);
    await page.evaluate(() => window.conversationScrollQA.appendUser());
    await unchanged(page, browsingPosition);
    await page.locator(".jump-latest").click();
    await atBottom(page);
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Live user request",
    );
    await page.evaluate(() => window.conversationScrollQA.append());
    await atBottom(page);

    // A deep dive has its own cursor; a delayed lookup cannot
    // replace its preview or move focus/scroll in the new conversation.
    if (mobile)
      await page
        .getByRole("button", { name: "Show sidebar", exact: true })
        .click();
    await page
      .locator(".sidebar .thread-card")
      .filter({ hasText: "builder" })
      .click();
    if (mobile)
      await page
        .getByRole("button", { name: "Talk to this thread", exact: true })
        .click();
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task builder:4",
    );
    for (const turn of [4, 3]) {
      await page.locator(".latest-user-message button").click();
      await expect(page.locator(".block-user:focus")).toContainText(
        `Task builder:${turn}`,
      );
      if (turn === 4) {
        await expect
          .poll(() =>
            page.evaluate(() => window.conversationScrollQA.pending()),
          )
          .toContain("builder:3");
        await page.evaluate(() =>
          window.conversationScrollQA.release("builder", 3),
        );
        await expect(page.locator(".latest-user-preview")).toHaveText(
          "Task builder:3",
        );
      }
    }
    await expect
      .poll(() => page.evaluate(() => window.conversationScrollQA.pending()))
      .toContain("builder:2");
    await page
      .locator(mobile ? ".topbar" : ".conversation-heading")
      .getByRole("button", {
        name: mobile ? "Return to orchestrator" : "Back to orchestrator",
        exact: true,
      })
      .click();
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:4",
    );
    await atBottom(page);
    const switchedPosition = await read(page);
    await page.evaluate(() =>
      window.conversationScrollQA.release("builder", 2),
    );
    await unchanged(page, switchedPosition);
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:4",
    );
    console.log(
      `PASS ${mobile ? "phone" : "desktop"}: virtual jumps step backward, exact boundaries update within a row, background/live changes preserve position and focus, earliest boundary and deep-dive switches are isolated`,
    );

    await page.goto(url + "?with-user&sparse");
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:4",
    );
    // Reach the boundary manually, without activating the shortcut.
    await scrollTo(page, 0);
    await pending(page, 3);
    const lookupPosition = await read(page);
    await release(page, 3);
    await unchanged(page, lookupPosition);
    await pending(page, 2);
    await release(page, 2);
    await pending(page, "2:next");
    await unchanged(page, lookupPosition);
    await page.evaluate(() => window.conversationScrollQA.releaseNext());
    await pending(page, 1);
    await unchanged(page, lookupPosition);
    await release(page, 1);
    await unchanged(page, lookupPosition);
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:1",
    );
    console.log(
      `PASS ${mobile ? "phone" : "desktop"}: manual boundary crossing finds a predecessor across automatic turns and paginated history without moving the viewport`,
    );

    await page.goto(url + "?with-user&short");
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:4",
    );
    for (const turn of [4, 3, 2, 1]) {
      await page.locator(".latest-user-message button").focus();
      await page.keyboard.press("Enter");
      await expect(page.locator(".block-user:focus-visible")).toContainText(
        `Task orch:${turn}`,
      );
      await expect(page.locator(".latest-user-preview")).toHaveText(
        turn > 1 ? `Task orch:${turn - 1}` : "No earlier messages from you",
      );
    }
    console.log(
      `PASS ${mobile ? "phone" : "desktop"}: clamped jumps in a short history still step back once per keyboard activation`,
    );
    await page.goto(url + "?with-user&short");
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:4",
    );
    await page.locator(".latest-user-message button").click();
    await expect(page.locator(".block-user:focus")).toContainText(
      "Task orch:4",
    );
    const shortOffset = await page
      .locator(".conversation-scroll")
      .evaluate((node) => node.scrollTop);
    await page.evaluate(() => window.conversationScrollQA.append());
    await page.waitForTimeout(250);
    expect(
      await page
        .locator(".conversation-scroll")
        .evaluate((node) => node.scrollTop),
    ).toBe(shortOffset);
    // Actual manual navigation back to the bottom resumes follow after a jump.
    await page.locator(".conversation-scroll").dispatchEvent("wheel");
    await scrollTo(page, 1);
    await atBottom(page);
    await page.evaluate(() => window.conversationScrollQA.append());
    await atBottom(page);
    console.log(
      `PASS ${mobile ? "phone" : "desktop"}: a clamped jump pauses auto-follow; manual navigation to the bottom resumes it`,
    );
    expect(errors).toEqual([]);
    await page.close();
  }

  // All three reviewed features together, with the real DOM and virtualizer.
  {
    const page = await browser.newPage({
      viewport: { width: 1440, height: 900 },
      reducedMotion: "reduce",
    });
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(url + "?with-user&combined");
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:4",
    );
    await scrollTo(page, 1);
    const box = page.locator('section.thread-message[aria-label="builder"]');
    await expect(box).toBeVisible();
    await expect(box.locator("strong")).toHaveText("Ready");
    await expect(box.locator("code")).toHaveText("code");
    await expect(box.locator("a")).toHaveAttribute(
      "href",
      "https://example.com/guide",
    );
    await expect(
      page.locator('section.thread-message[aria-label="reviewer"] li'),
    ).toHaveCount(2);
    await page
      .getByRole("textbox", { name: "Message orchestrator", exact: true })
      .fill("Orchestrator draft");
    await page
      .locator(".sidebar .thread-card")
      .filter({ hasText: "builder" })
      .click();
    const composer = page.getByRole("textbox", {
      name: "Message builder directly",
      exact: true,
    });
    await expect(composer).toBeVisible();
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task builder:4",
    );
    await composer.fill("Builder draft");
    await page
      .getByRole("button", { name: "Show thread details", exact: true })
      .click();
    await expect(page.locator("#twr-inspector")).toBeVisible();
    await expect(composer).toHaveValue("Builder draft");
    await page.locator(".latest-user-message button").click();
    await expect(page.locator(".block-user:focus")).toContainText(
      "Task builder:4",
    );
    await page
      .getByRole("button", { name: "Close thread details", exact: true })
      .click();
    await expect(
      page.getByRole("button", { name: "Show thread details", exact: true }),
    ).toBeFocused();
    await expect(composer).toHaveValue("Builder draft");
    await scrollTo(page, 1);
    // Protocol tags belong only to orchestrator replies.
    await expect(page.locator("section.thread-message")).toHaveCount(0);
    await page
      .getByRole("button", { name: "Back to orchestrator", exact: true })
      .click();
    await expect(
      page.getByRole("textbox", { name: "Message orchestrator", exact: true }),
    ).toHaveValue("Orchestrator draft");
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:4",
    );
    await scrollTo(page, 1);
    await expect(box).toBeVisible();
    await page.locator(".latest-user-message button").click();
    await expect(page.locator(".block-user:focus")).toContainText(
      "Task orch:4",
    );
    expect(errors).toEqual([]);
    await page.screenshot({ path: "test-results/combined-ui-jump.png" });
    console.log(
      "PASS combined UI: named Markdown boxes and shared references, single-click destination changes, per-thread drafts and jump targets, details toggle/focus, and orchestrator-only tag parsing",
    );
    await page.close();
  }

  // Chromium does not generate iOS momentum. Select the real virtualizer's iOS
  // branch via navigator, then drive its touch/tail lifecycle with synthetic
  // events. Assert that a prepend really is held (indexes advance while the
  // DOM scroll position stays put); viewport size alone cannot cover this bug.
  for (const gesture of ["drag", "momentum"]) {
    const page = await browser.newPage({
      viewport: { width: 390, height: 844 },
      userAgent:
        "Mozilla/5.0 (iPhone; CPU iPhone OS 27_0 like Mac OS X) AppleWebKit/605.1.15 Version/27.0 Mobile/15E148 Safari/604.1",
      hasTouch: true,
      reducedMotion: "reduce",
    });
    page.setDefaultTimeout(5000);
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const touch = (type) =>
      page.locator(".conversation-scroll").dispatchEvent(type);
    await page.goto(url);
    await pending(page, 3);
    await scrollTo(page, 1);
    await scrollTo(page, 1);
    await touch("touchstart");
    await scrollTo(page, 0.6);
    await scrollTo(page, 0.4);
    const heldBefore = await read(page);
    await release(page, 3);
    await unchanged(page, heldBefore);
    const heldAfter = await read(page);
    expect(heldAfter.index - heldBefore.index).toBe(61);
    expect(
      Math.abs(heldAfter.scrollTop - heldBefore.scrollTop),
    ).toBeLessThanOrEqual(1);
    await touch("touchend");
    await unchanged(page, heldBefore); // The deferred prepend also lands safely.
    await pending(page, 2);
    await release(page, 2);
    await pending(page, 1);
    await frames(page);

    await touch("touchstart");
    await scrollTo(page, 0.5);
    await scrollTo(page, 0.35);
    if (gesture === "momentum") {
      await touch("touchend");
      // A scroll frame after touchend extends the virtualizer's momentum tail.
      await page.locator(".conversation-scroll").evaluate((node) => {
        node.scrollTop -= 37;
      });
      await frames(page);
    }
    const before = await read(page);
    await release(page, 1); // Removes Load earlier while the prepend is held.
    await frames(page);
    const after = await read(page);
    expect(after.text).toBe(before.text);
    expect(after.index - before.index).toBe(61);
    expect(after.margin).toBeLessThan(before.margin);
    // Only the control's height is written to the DOM here. The newly inserted
    // rows' height remains held internally until the gesture/tail settles.
    expect(
      Math.abs(
        after.scrollTop - before.scrollTop - (after.margin - before.margin),
      ),
    ).toBeLessThanOrEqual(1);
    await unchanged(page, before);
    if (gesture === "drag") {
      await touch("touchend");
      await unchanged(page, before);
    }
    await expect(page.locator(".load-earlier")).toHaveCount(0);
    expect(errors).toEqual([]);
    console.log(
      `PASS synthetic iOS ${gesture}: verified held prepend, final-control removal, and unchanged message/pixel offset before and after gesture settlement`,
    );

    await page.goto(url + "?with-user");
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:4",
    );
    await page.locator(".load-earlier").click();
    await pending(page, 3);
    await touch("touchstart");
    await scrollTo(page, 0.6);
    await scrollTo(page, 0.4);
    if (gesture === "momentum") {
      await touch("touchend");
      await page.locator(".conversation-scroll").evaluate((node) => {
        node.scrollTop -= 37;
      });
      await frames(page);
    }
    const userHeldBefore = await read(page);
    await release(page, 3);
    await frames(page);
    const userHeldAfter = await read(page);
    expect(userHeldAfter.index - userHeldBefore.index).toBe(61);
    expect(
      Math.abs(userHeldAfter.scrollTop - userHeldBefore.scrollTop),
    ).toBeLessThanOrEqual(1);
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:4",
    );
    await unchanged(page, userHeldBefore);
    if (gesture === "drag") await touch("touchend");
    await unchanged(page, userHeldBefore);
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:4",
    );
    expect(errors).toEqual([]);
    console.log(
      `PASS synthetic iOS ${gesture}: predecessor selection uses held layout offsets and stays stable through settlement`,
    );
    await page.close();
  }
} finally {
  await browser?.close();
  await server.close();
}
