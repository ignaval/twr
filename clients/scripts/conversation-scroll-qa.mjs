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
    for (const theme of ["light", "dark"]) {
      await page.evaluate((theme) => {
        document.documentElement.dataset.theme = theme;
      }, theme);
      for (const [name, color] of [
        ["builder", "rgb(0, 135, 175)"],
        ["reviewer", "rgb(255, 0, 255)"],
      ]) {
        const section = page.locator(
          `section.thread-message[aria-label="${name}"]`,
        );
        await expect(section).toHaveCSS("border-top-color", color);
        const card = page
          .locator(".sidebar .thread-card")
          .filter({ hasText: name });
        const ink = await card.evaluate((node) => {
          const probe = document.createElement("span");
          probe.className = "thread-ink";
          node.append(probe);
          const color = getComputedStyle(probe).color;
          probe.remove();
          return color;
        });
        await expect(section.locator(".thread-message-name")).toHaveCSS(
          "color",
          ink,
        );
      }
      const unknown = page.locator(
        'section.thread-message[aria-label="missing"]',
      );
      const neutral = await page
        .locator(".app")
        .evaluate((node) =>
          getComputedStyle(node).getPropertyValue("--border").trim(),
        );
      // Compare computed colors so CSS hex/rgb/color-mix serialization is irrelevant.
      expect(
        await unknown.evaluate((node, neutral) => {
          const probe = document.createElement("span");
          probe.style.color = neutral;
          node.append(probe);
          const matches =
            getComputedStyle(node).borderTopColor ===
            getComputedStyle(probe).color;
          probe.remove();
          return matches;
        }, neutral),
      ).toBe(true);
      await page.screenshot({
        path: `test-results/thread-box-colors-${theme}.png`,
      });
    }
    console.log(
      "PASS thread box colors: distinct assigned palette colors, readable theme inks, and neutral unknown-thread borders in light and dark",
    );
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

  // Keep the same reply mounted while live snapshots change its thread identity.
  // A new thread may reuse a deleted thread's name or alias, but not its history.
  for (const theme of ["light", "dark"]) {
    const page = await browser.newPage({
      viewport: { width: 1440, height: 900 },
    });
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(url + "?with-user&combined");
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:4",
    );
    await scrollTo(page, 1);
    await page.evaluate((theme) => {
      document.documentElement.dataset.theme = theme;
    }, theme);
    const box = page
      .locator('section.thread-message[aria-label="builder"]')
      .first();
    await expect(box).toHaveCSS("border-top-color", "rgb(0, 135, 175)");
    await box.evaluate((node) => {
      node.dataset.mounted = "original";
    });
    const threads = await page.evaluate(() =>
      window.conversationScrollQA.threads(),
    );
    const original = threads.find((thread) => thread.id === "builder");
    const others = threads.filter((thread) => thread.id !== "builder");
    const update = (workers) =>
      page.evaluate(
        (threads) => window.conversationScrollQA.setThreads(threads),
        [...others, ...workers],
      );
    const neutral = async () => {
      await expect(box).not.toHaveAttribute("style", /--thread-color:/);
      await expect(box).toHaveAttribute("data-mounted", "original");
      await expect(box.locator("strong")).toHaveText("Ready");
      await expect(box.locator("a")).toHaveAttribute(
        "href",
        "https://example.com/guide",
      );
    };
    // Missing metadata can arrive later through a snapshot, without a text update.
    await update([{ ...original, created_at: null }]);
    await neutral();
    await update([
      { ...original, name: "renamed-builder", former_names: ["builder"] },
    ]);
    await expect(box).toHaveCSS("border-top-color", "rgb(0, 135, 175)");
    await expect(box).toHaveAttribute("data-mounted", "original");
    await update([]);
    await neutral();
    const replacement = {
      ...original,
      id: "replacement",
      created_at: "2026-11-17T12:00:01Z",
      accent: "#ff00ff",
    };
    await update([replacement]);
    await neutral();
    await update([
      { ...replacement, name: "replacement", aliases: ["builder"] },
    ]);
    await neutral();
    // Force a history remount by leaving the conversation and coming back.
    await page
      .locator(".sidebar .thread-card-title b")
      .getByText("replacement", { exact: true })
      .click();
    await expect(
      page.getByRole("textbox", {
        name: "Message replacement directly",
        exact: true,
      }),
    ).toBeVisible();
    await page
      .getByRole("button", { name: "Back to orchestrator", exact: true })
      .click();
    await scrollTo(page, 1);
    await expect(box).not.toHaveAttribute("style", /--thread-color:/);
    // A reply actually sent after replacement creation may use its alias/color.
    await page.evaluate(() =>
      window.conversationScrollQA.reply(
        '<thread name="builder">\nNew worker update.\n</thread>',
        "2026-11-17T12:00:02Z",
      ),
    );
    await scrollTo(page, 1);
    const boxes = page.locator('section.thread-message[aria-label="builder"]');
    await expect(boxes).toHaveCount(2);
    await expect(boxes.first()).not.toHaveAttribute("style", /--thread-color:/);
    await expect(boxes.last()).toHaveCSS(
      "border-top-color",
      "rgb(255, 0, 255)",
    );
    expect(errors).toEqual([]);
    console.log(
      `PASS ${theme}: mounted metadata/rename/deletion updates, name and alias reuse stay neutral, history remount stays safe, and new replies resolve the replacement`,
    );
    await page.close();
  }

  // Live first deltas and fetched final-only history have different timestamps.
  // Replace the worker in between, then force an actual filtered RPC history read.
  for (const theme of ["light", "dark"]) {
    const page = await browser.newPage({
      viewport: { width: 1440, height: 900 },
    });
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(url + "?with-user&combined");
    await expect(page.locator(".latest-user-preview")).toHaveText(
      "Task orch:4",
    );
    await scrollTo(page, 1);
    await page.evaluate((theme) => {
      document.documentElement.dataset.theme = theme;
    }, theme);
    const threads = await page.evaluate(() =>
      window.conversationScrollQA.threads(),
    );
    const original = threads.find((thread) => thread.id === "builder");
    const others = threads.filter((thread) => thread.id !== "builder");
    const update = (workers) =>
      page.evaluate(
        (threads) => window.conversationScrollQA.setThreads(threads),
        [...others, ...workers],
      );
    await update([{ ...original, aliases: ["backend"] }]);
    const text =
      '<thread name="builder">\n**Streaming name update** [docs][d]\n</thread>\n<thread name="backend">\n**Streaming alias update**\n</thread>\n\n[d]: https://example.com/stream';
    await page.evaluate((text) => {
      window.conversationScrollQA.stream(
        "turn.started",
        { from: "user", prompt: "Streaming reuse scenario" },
        "2026-11-17T12:00:00Z",
      );
      window.conversationScrollQA.stream(
        "agent.text",
        { text, delta: true },
        "2026-11-17T12:00:01Z",
      );
    }, text);
    await scrollTo(page, 1);
    const boxes = page
      .locator("section.thread-message")
      .filter({ hasText: /Streaming (name|alias) update/ });
    await expect(boxes).toHaveCount(2);
    for (const box of await boxes.all()) {
      await expect(box).toHaveCSS("border-top-color", "rgb(0, 135, 175)");
      await box.evaluate((node) => {
        node.dataset.mounted = "streaming";
      });
    }
    await update([]);
    for (const box of await boxes.all())
      await expect(box).not.toHaveAttribute("style", /--thread-color:/);
    await update([
      {
        ...original,
        id: "replacement",
        aliases: ["backend"],
        accent: "#ff00ff",
        created_at: "2026-11-17T12:00:02Z",
      },
    ]);
    await page.evaluate(
      (text) =>
        window.conversationScrollQA.stream(
          "agent.text",
          { text, final: true },
          "2026-11-17T12:00:03Z",
        ),
      text,
    );
    await expect(
      page
        .locator(".block-assistant")
        .filter({ has: boxes.first() })
        .locator(".spin"),
    ).toHaveCount(0);
    for (const box of await boxes.all()) {
      await expect(box).not.toHaveAttribute("style", /--thread-color:/);
      await expect(box).toHaveAttribute("data-mounted", "streaming");
    }
    const readsBefore = await page.evaluate(
      () => window.conversationScrollQA.historyReads().length,
    );
    await page
      .locator(".sidebar .thread-card-title b")
      .getByText("reviewer", { exact: true })
      .click();
    await expect(
      page.getByRole("textbox", {
        name: "Message reviewer directly",
        exact: true,
      }),
    ).toBeVisible();
    await page
      .getByRole("button", { name: "Back to orchestrator", exact: true })
      .click();
    await expect
      .poll(() =>
        page.evaluate(() => window.conversationScrollQA.historyReads().length),
      )
      .toBeGreaterThan(readsBefore);
    const fetched = await page.evaluate(() =>
      window.conversationScrollQA.historyReads().at(-1),
    );
    expect(
      fetched.map((event) => [event.kind, event.ts, event.data.delta]),
    ).toEqual([
      ["turn.started", "2026-11-17T12:00:00Z", undefined],
      ["agent.text", "2026-11-17T12:00:03Z", undefined],
    ]);
    await scrollTo(page, 1);
    await expect(boxes).toHaveCount(2);
    for (const box of await boxes.all()) {
      await expect(box).not.toHaveAttribute("style", /--thread-color:/);
      await expect(box).not.toHaveAttribute("data-mounted", "streaming");
    }
    await expect(boxes.first().locator("a")).toHaveAttribute(
      "href",
      "https://example.com/stream",
    );
    await page.evaluate(() =>
      window.conversationScrollQA.reply(
        '<thread name="builder">\nNew turn name update.\n</thread>\n<thread name="backend">\nNew turn alias update.\n</thread>',
        "2026-11-17T12:00:04Z",
      ),
    );
    await scrollTo(page, 1);
    const newBoxes = page
      .locator("section.thread-message")
      .filter({ hasText: /New turn (name|alias) update/ });
    await expect(newBoxes).toHaveCount(2);
    for (const box of await newBoxes.all())
      await expect(box).toHaveCSS("border-top-color", "rgb(255, 0, 255)");
    expect(errors).toEqual([]);
    console.log(
      `PASS ${theme}: replacement between first delta and final stays neutral after final-only RPC history reload for both name and alias; a later turn uses the replacement color`,
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
    await page.close();
  }
} finally {
  await browser?.close();
  await server.close();
}
