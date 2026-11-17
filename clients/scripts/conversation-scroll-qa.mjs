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
  const read = (page) =>
    page.locator(".conversation-scroll").evaluate((scroll) => {
      const top = scroll.getBoundingClientRect().top;
      const message = [...scroll.querySelectorAll(".block-assistant")].find(
        (row) => {
          const rect = row.getBoundingClientRect();
          return rect.bottom > top + 1 && rect.top < top + scroll.clientHeight;
        },
      );
      if (!message) throw new Error("No visible message to anchor");
      return {
        text: message.querySelector(".markdown p").textContent,
        offset: message.getBoundingClientRect().top - top,
        index: Number(message.dataset.index),
        scrollTop: scroll.scrollTop,
        margin: scroll.querySelector(".transcript-virtual").offsetTop,
      };
    });
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
    expect(errors).toEqual([]);
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
    await page.close();
  }
} finally {
  await browser?.close();
  await server.close();
}
