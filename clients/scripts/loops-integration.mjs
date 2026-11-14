// Review and implementation loops in the real Electron app against a real Rust hub
// (docs/desktop.md › Loops,
// docs/design/loops.md › Desktop and mobile): a fake thread in a git repository, and a fake
// Codex reviewer (`TWR_CODEX_BIN`) that holds a run while `reviewers/hold.<n>` exists and
// fails it while `reviewers/fail.<n>` does. The hub is told Codex is installed and logged in
// (`TWR_TEST_AGENTS`, honoured by a test's throwaway hub only). The fake runtime echoes each
// prompt, so the loop's own prompts answer themselves: the templates in
// `<TWR_HOME>/loops/templates/` end with the block each step asks for.
//
// `TWR_LOOPS_SHOTS=<dir>` also saves, at every step, the window in the light and the dark
// theme at the default size and at the minimum width (`NN-step-<light|dark>.png`,
// `NN-step-<light|dark>-min.png`) and the accessibility tree of the step's region
// (`NN-step.aria.txt`), for a person to look at.
//
// `TWR_LOOPS_WINDOW=<width>x<height>` runs it in a window of that content size (a CI
// runner's screen is smaller than the default 1360×900: `1024x674`), set right after
// launch, before anything is measured.
import { stopFixture } from "./fixture-cleanup.mjs";
import { _electron as electron } from "@playwright/test";
import { spawn, execFileSync } from "node:child_process";
import {
  mkdtempSync,
  realpathSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  rmSync,
  chmodSync,
  existsSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { createServer } from "node:net";
import { join, resolve } from "node:path";
import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";

if (process.env.TWR_DESKTOP_EXECUTABLE) {
  // The fake reviewer needs a debug hub's `TWR_TEST_AGENTS`, which a packaged build lacks.
  console.log(
    "SKIP: review loops run against the debug build only (TWR_DESKTOP_EXECUTABLE is set)",
  );
  process.exit(0);
}
const shots = process.env.TWR_LOOPS_SHOTS
  ? resolve(process.env.TWR_LOOPS_SHOTS)
  : undefined;
if (shots) mkdirSync(shots, { recursive: true });
const windowSize = process.env.TWR_LOOPS_WINDOW
  ? process.env.TWR_LOOPS_WINDOW.match(/^(\d+)x(\d+)$/)
      ?.slice(1)
      .map(Number)
  : undefined;
if (process.env.TWR_LOOPS_WINDOW && !windowSize)
  throw new Error("TWR_LOOPS_WINDOW is <width>x<height>, such as 1024x674");

const home = realpathSync(mkdtempSync(join(tmpdir(), "twr-loops-")));
const repo = join(home, "repo");
// Step 11's remote box: a dial-in runner's own TWR_HOME.
const boxHome = join(home, "box");
mkdirSync(repo);
const git = (...args) =>
  execFileSync(
    "git",
    [
      "-C",
      repo,
      "-c",
      "user.name=Desktop Test",
      "-c",
      "user.email=desktop@example.invalid",
      ...args,
    ],
    { stdio: "ignore" },
  );
execFileSync("git", ["init", "-b", "main", repo], { stdio: "ignore" });
writeFileSync(join(repo, "lib.rs"), "pub fn f() {}\n");
git("add", "-A");
git("commit", "-m", "Initial");
// One sweep audit at a time (step 10): the fake reviewer's runs keep their order.
const config = (offer) =>
  `default_runtime = "fake"\n[projects.demo]\nrepo = ${JSON.stringify(repo)}\n[orchestrator]\nautostart = false\n[threads]\nidle_compaction = "off"\n[tui]\nsuggest = false\nsummaries = false\n[loops]\nmax_audit_runs = 1\n${offer === false ? "offer_review = false\n" : ""}`;
// The hub starts with the offer off; step 1 turns it on.
writeFileSync(join(home, "config.toml"), config(false), { mode: 0o600 });
const reviewers = join(home, "reviewers");
mkdirSync(reviewers);
const codex = join(reviewers, "codex");
writeFileSync(
  codex,
  `#!/bin/sh
here=$(dirname "$0")
n=$(( $(cat "$here/count" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$here/count"
cat > /dev/null
while [ -f "$here/hold.$n" ]; do sleep 0.05; done
if [ -f "$here/fail.$n" ]; then echo "fake reviewer failed run $n" >&2; exit 1; fi
out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
if [ -f "$here/answers/$n.json" ]; then
  cp "$here/answers/$n.json" "$out"
elif [ "$n" = 1 ]; then
  printf '%s' '{"findings": [{"severity": "major", "location": "lib.rs:1", "problem": "it breaks on empty input", "fix": "guard it"}]}' > "$out"
else
  printf '%s' '{"findings": []}' > "$out"
fi
echo '{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":10}}'
`,
);
chmodSync(codex, 0o755);
const hold = (n) => writeFileSync(join(reviewers, `hold.${n}`), "");
const release = (n) => rmSync(join(reviewers, `hold.${n}`), { force: true });
const failRun = (n) => writeFileSync(join(reviewers, `fail.${n}`), "");
/** Review run `n` answers `findings` (by default run 1 finds one problem, later runs none). */
const answer = (n, findings) => {
  mkdirSync(join(reviewers, "answers"), { recursive: true });
  writeFileSync(
    join(reviewers, "answers", `${n}.json`),
    JSON.stringify({ findings }),
  );
};
const runs = () => {
  try {
    return Number(readFileSync(join(reviewers, "count"), "utf8").trim());
  } catch {
    return 0;
  }
};
const templates = join(home, "loops", "templates");
mkdirSync(templates, { recursive: true });
writeFileSync(
  join(templates, "goal.md"),
  "Gates are green.\n```twr-goal\nHarden the change against bad input.\n```\n",
);
writeFileSync(
  join(templates, "triage.md"),
  '{{findings}}\nTriaged.\n```twr-triage\n{"items": [{"id": "{{finding_ids}}", "verdict": "dismissed", "reason": "empty input never reaches it"}], "gates": {"ran": ["true"], "result": "pass"}}\n```\n',
);
writeFileSync(
  join(templates, "report.md"),
  "The loop reviewed the change; nothing is left open.\n",
);
// An implementation loop's steps (step 8): the plan, the implement turn that writes a file,
// and the report turn, which sleeps while the script commits the work for it (the fake
// runtime runs no git), then names a commit (the hub checks the repository, not the sha).
// `{{commit}}` comes before the block: the fake echoes the wording's indented example as a
// fence, and the hub reads the last one.
const PLAN =
  'Plan: a CSV export on the reports page.\n```twr-plan\n{"contract_risk": false, "reason": "UI only: a new button"}\n```\n';
const IMPLEMENT =
  '!write feature.rs pub fn export_csv() {}\nImplemented.\n```twr-implement\n{"status": "done", "gates": {"ran": ["true"], "result": "pass"}}\n```\n';
writeFileSync(join(templates, "plan.md"), PLAN);
writeFileSync(join(templates, "implement.md"), IMPLEMENT);
writeFileSync(
  join(templates, "impl-report.md"),
  '!sleep 4000\nBuilt the CSV export.\n{{commit}}\n```twr-commit\n{"commits": [{"repo": "demo", "sha": "abc1234"}]}\n```\n',
);
// A sweep audited by runs (step 10): a triage that fixes the round's finding as a new class
// (so the `medium` profile sweeps), the rules turn naming two rules, and the verification
// turn's verdict on each.
const FIX_TRIAGE =
  '{{findings}}\nTriaged.\n```twr-triage\n{"items": [{"id": "{{finding_ids}}", "verdict": "fixed", "class": "new-class", "rule": "guard-input"}], "gates": {"ran": ["true"], "result": "pass"}}\n```\n';
const RULES =
  'Named.\n```twr-rules\n{"rules": [{"name": "guard-input", "rule": "Every parser guards empty input.", "where": "lib.rs"}, {"name": "trim-input", "rule": "Every parser trims its input.", "where": "everywhere the diff touches"}]}\n```\n';
const SWEEP_VERIFY =
  'Verified.\n```twr-sweep\n{"rules": [{"rule": "guard-input", "result": "fixed"}, {"rule": "trim-input", "result": "checked clean"}], "gates": {"ran": ["true"], "result": "pass"}}\n```\n';
const tokenFile = join(home, "test-user.token");
writeFileSync(tokenFile, randomUUID(), { mode: 0o600 });
const bin = resolve("../target/debug");
const env = {
  ...process.env,
  TWR_HOME: home,
  TWR_TEST_USER_TOKEN_FILE: tokenFile,
  TWR_ENABLE_FAKE: "1",
  TWR_DESKTOP_TEST_AUTOMATION: "1",
  TWR_DESKTOP_BINARY_DIR: bin,
  TWR_DESKTOP_TEST_USER_DATA: join(home, "electron"),
  TWR_WORKSPACE: "default",
  TWR_AGENT_PROBE: "off",
  TWR_BOX_PROBE: "off",
  TWR_CODEX_BIN: codex,
  TWR_TEST_AGENTS: JSON.stringify([
    { runtime: "codex", found: true, login: "logged_in" },
    { runtime: "claude-code", found: true, login: "logged_in" },
    { runtime: "grok-build", found: false, login: "unknown" },
  ]),
  HOME: home,
  SHELL: "/bin/sh",
  XDG_CONFIG_HOME: join(home, "config"),
  XDG_CACHE_HOME: join(home, "cache"),
};
delete env.ELECTRON_RUN_AS_NODE;
delete env.RUST_LOG;
delete env.VISUAL;
const twr = (...args) =>
  execFileSync(join(bin, "twr"), args, {
    env,
    encoding: "utf8",
    stdio: "pipe",
  });
const twrJson = (...args) => JSON.parse(twr(...args, "--json"));
const thread = (name) => twrJson("thread", "list").find((t) => t.name === name);
async function until(check, message, ms = 30000) {
  const deadline = Date.now() + ms;
  for (;;) {
    const value = await check();
    if (value) return value;
    if (Date.now() > deadline) throw new Error(message);
    await new Promise((r) => setTimeout(r, 150));
  }
}

let application;
let applicationProcess;
let hub;
let hubLog = "";
let runner;
let runnerLog = "";
const errors = [];
const startHub = async () => {
  hub = spawn(join(bin, "twr"), ["hub"], {
    env,
    stdio: ["ignore", "ignore", "pipe"],
  });
  hub.stderr.on("data", (data) => (hubLog += data));
  await until(() => {
    try {
      twr("info", "--json");
      return true;
    } catch {
      return false;
    }
  }, `The hub did not start: ${hubLog}`);
};
try {
  await startHub();
  application = await electron.launch({
    args: ["."],
    cwd: resolve("."),
    env,
    timeout: 60000,
  });
  applicationProcess = application.process();
  const page = await application.firstWindow();
  page.on("pageerror", (error) => errors.push(error.message));
  if (windowSize) {
    const [width, height] = windowSize;
    await application.evaluate(
      ({ BrowserWindow }, [width, height]) =>
        BrowserWindow.getAllWindows()[0].setContentSize(width, height),
      windowSize,
    );
    await until(
      () =>
        page.evaluate(
          ([width, height]) => innerWidth === width && innerHeight === height,
          windowSize,
        ),
      `The window never took the size ${width}x${height}`,
      10000,
    );
  }
  const connected = () =>
    page
      .locator(".connection-status")
      .filter({ hasText: "Hub connected" })
      .waitFor({ timeout: 30000 });
  await connected();
  const defaultSize = await page.evaluate(() => ({
    width: innerWidth,
    height: innerHeight,
  }));
  console.log("Connected to real hub; window", defaultSize);

  const theme = (value) =>
    page.evaluate((value) => {
      localStorage.setItem("twr.theme", value);
      document.documentElement.dataset.theme = value;
      window.dispatchEvent(new Event("twr:appearance"));
    }, value);
  /**
   * Saves the window in both themes (and at the minimum width, `narrow` navigating there
   * first) and the accessibility tree of `region`.
   */
  async function capture(name, region, { narrow, restore } = {}) {
    if (!shots) return;
    if (region) {
      const aria = await region.ariaSnapshot({ timeout: 5000 });
      writeFileSync(join(shots, `${name}.aria.txt`), aria + "\n");
    }
    for (const value of ["light", "dark"]) {
      await theme(value);
      await page.waitForTimeout(150);
      await page.screenshot({ path: join(shots, `${name}-${value}.png`) });
    }
    if (narrow !== false) {
      await page.setViewportSize({ width: 420, height: 800 });
      await page.waitForTimeout(200);
      if (narrow) await narrow();
      await page.waitForTimeout(200);
      for (const value of ["light", "dark"]) {
        await theme(value);
        await page.waitForTimeout(150);
        await page.screenshot({
          path: join(shots, `${name}-${value}-min.png`),
        });
      }
      await page.setViewportSize(defaultSize);
      await page.waitForTimeout(200);
      if (restore) await restore();
      const show = page.getByRole("button", {
        name: "Show sidebar",
        exact: true,
      });
      if (
        (await show.isVisible()) &&
        !(await show.evaluate((n) => !!n.closest("[inert]")))
      )
        await show.click();
    }
    await theme("dark");
  }

  // OS notifications: automation records what would be posted (desktop/app-services.ts).
  const send = (event) =>
    application.evaluate(
      ({ BrowserWindow }, event) =>
        BrowserWindow.getAllWindows()[0].webContents.send("twr:event", event),
      event,
    );
  const notifications = () =>
    application.evaluate(() =>
      globalThis.__twrTest.notifications.map(({ title, body }) => ({
        title,
        body,
      })),
    );
  const hidden = async (run) => {
    await application.evaluate(({ BrowserWindow }) =>
      BrowserWindow.getAllWindows()[0].hide(),
    );
    try {
      return await run();
    } finally {
      await application.evaluate(({ BrowserWindow }) => {
        const window = BrowserWindow.getAllWindows()[0];
        window.show();
        window.focus();
      });
    }
  };
  const notified = (body) =>
    until(async () => {
      await send({ type: "inbox-poll" });
      return (await notifications()).find((n) => n.body === body);
    }, `No notification "${body}"`);

  // Locators and helpers ------------------------------------------------------------
  /** Navigating at the narrow width may have closed the sidebar: open it again. */
  const sidebar = async () => {
    const show = page.getByRole("button", {
      name: "Show sidebar",
      exact: true,
    });
    if (await show.isVisible()) await show.click();
  };
  const sidebarCard = () =>
    page.locator(".thread-list .thread-item").filter({ hasText: "api" });
  const inspector = page.locator("#twr-inspector");
  const loopsSection = inspector.locator(".inspector-loops");
  const setupIn = (where) => where.locator(".attention-loop-setup");
  // Everything the live regions say, kept: they clear after a few seconds.
  await page.evaluate(() => {
    window.__said = [];
    new MutationObserver(() => {
      for (const node of document.querySelectorAll("[data-twr-live]"))
        if (node.textContent && window.__said.at(-1) !== node.textContent)
          window.__said.push(node.textContent);
    }).observe(document.body, {
      subtree: true,
      childList: true,
      characterData: true,
    });
  });
  const live = () => page.evaluate(() => window.__said.join(" | "));
  /** Waits until a live region has said `text` (they clear after a while). */
  const announced = (text) =>
    until(
      async () => (await live()).includes(text),
      `Never announced: ${text}`,
    );
  const select = async () => {
    await sidebar();
    await sidebarCard().locator(".thread-card").click();
    await inspector.waitFor();
  };
  const talk = async () => {
    await select();
    await page.getByRole("button", { name: "Talk to this thread" }).click();
    await page.getByRole("textbox", { name: "Message api directly" }).waitFor();
  };
  /** Back from the narrow width's sidebar, to the thread's inspector. */
  const back = async () => {
    await page.keyboard.press("Escape");
    await select();
  };
  const toOrchestrator = async () => {
    await sidebar();
    await page
      .locator(".sidebar")
      .getByRole("button", { name: "Orchestrator", exact: true })
      .first()
      .click();
  };
  const nav = async (name) => {
    await sidebar();
    await page
      .locator(".sidebar nav")
      .first()
      .getByRole("button", { name })
      .first()
      .click();
  };
  /**
   * At the minimum width the selected thread's inspector shows over the conversation: the
   * sidebar's drawer, if open, is closed so that it does not cover it, and `region` is
   * brought into view.
   */
  const inspectorNarrow = (region) => async () => {
    const hide = page.getByRole("button", {
      name: "Hide sidebar",
      exact: true,
    });
    if (await hide.isVisible()) await hide.click();
    await inspector.waitFor();
    await region?.evaluate((n) => n.scrollIntoView({ block: "start" }));
  };
  /** Closes the passing notices (they go by themselves after a few seconds). */
  const dismissNotices = async () => {
    for (const button of await page
      .getByRole("button", { name: "Dismiss notice" })
      .all())
      if (await button.isVisible()) await button.click();
  };
  const compact = (name) =>
    page
      .locator(".compact-nav")
      .getByRole("button", { name, exact: false })
      .first()
      .click();
  /** WCAG contrast of an element's text on its own (opaque) background. */
  // Buttons ease their colours over 120 ms: a theme switch is measured once those
  // transitions have ended, never part-way, where text and fill pass through the same grey.
  const contrast = (locator) =>
    locator.evaluate(async (node) => {
      await new Promise((done) =>
        requestAnimationFrame(() => requestAnimationFrame(done)),
      );
      await Promise.all(
        node.getAnimations().map((a) => a.finished.catch(() => {})),
      );
      const rgb = (value) =>
        value
          .match(/[\d.]+/g)
          .slice(0, 3)
          .map(Number);
      const lum = (c) =>
        c
          .map((v) => v / 255)
          .map((v) => (v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4))
          .reduce((sum, v, i) => sum + v * [0.2126, 0.7152, 0.0722][i], 0);
      const style = getComputedStyle(node);
      const [a, b] = [lum(rgb(style.color)), lum(rgb(style.backgroundColor))];
      return (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
    });
  /** Every control in an aria snapshot has a role and a name. */
  const assertNamed = (aria, where) => {
    const nameless = aria
      .split("\n")
      .filter((line) =>
        /^\s*- (button|combobox|textbox|checkbox|link|radio|switch)\s*(\[[^\]]*\])?\s*(:.*)?$/.test(
          line,
        ),
      );
    assert.deepEqual(nameless, [], `${where}: controls without a name`);
  };
  const ariaOf = async (name, region) => {
    const aria = await region.ariaSnapshot({ timeout: 5000 });
    assertNamed(aria, name);
    if (shots) writeFileSync(join(shots, `${name}.aria.txt`), aria + "\n");
    return aria;
  };
  const items = () =>
    page.evaluate(() => window.twr.rpc("attention.list", { all: true }));
  const setupItem = async () =>
    (await items()).find((a) => a.thread !== "hub" && a.reason === "review");
  /** The element with keyboard focus, as a short description. */
  const focused = () =>
    page.evaluate(() => {
      const el = document.activeElement;
      if (!el) return "";
      const style = getComputedStyle(el);
      return {
        tag: el.tagName.toLowerCase(),
        text: (el.getAttribute("aria-label") || el.textContent || "").trim(),
        visible:
          style.outlineStyle !== "none" && parseFloat(style.outlineWidth) > 0,
      };
    });

  /**
   * A request in the conversation as a person finds it, whatever the window's size: the
   * requests take at most 35% of the conversation and scroll in their own area, so the
   * composer stays in view; while the card goes on below that area's fold, a cue at its
   * bottom edge says so, gone at the end; Tab from the card reaches its first answer and
   * brings it into view. Leaves the area scrolled to its top, as it was found.
   */
  const inlineRequest = async (card, where) => {
    const area = page.locator(".conversation-attention");
    const settled = () =>
      page.evaluate(
        () =>
          new Promise((done) =>
            requestAnimationFrame(() => requestAnimationFrame(done)),
          ),
      );
    const look = () =>
      area.evaluate((n) => ({
        scrollable: n.scrollHeight > n.clientHeight + 1,
        more: n.dataset.more,
        cue: getComputedStyle(n, "::after").content,
        send: document
          .querySelector('button[aria-label="Send message"]')
          .getBoundingClientRect().bottom,
        height: innerHeight,
      }));
    await area.evaluate((n) => n.scrollTo({ top: 0 }));
    await settled();
    const top = await look();
    assert(
      top.send <= top.height + 1,
      `${where}: the composer stays in view under the request`,
    );
    assert.equal(
      top.more,
      top.scrollable ? "below" : undefined,
      `${where}: a request that goes on below its area's fold says so`,
    );
    assert.equal(
      top.cue === "none",
      !top.scrollable,
      `${where}: the cue shows`,
    );
    if (top.scrollable) {
      await area.evaluate((n) => n.scrollTo({ top: n.scrollHeight }));
      await settled();
      const end = await look();
      assert.equal(end.more, undefined, `${where}: no cue at the area's end`);
      assert.equal(end.cue, "none");
      await area.evaluate((n) => n.scrollTo({ top: 0 }));
      await settled();
    }
    const first = card.locator(".attention-options button").first();
    await card.evaluate((n) => n.focus({ preventScroll: true }));
    for (let i = 0; i < 4; i++) {
      if (await first.evaluate((n) => n === document.activeElement)) break;
      await page.keyboard.press("Tab");
    }
    assert(
      await first.evaluate((n) => n === document.activeElement),
      `${where}: Tab from the card reaches its first answer`,
    );
    await settled();
    assert(
      await first.evaluate((n) => {
        const box = n.getBoundingClientRect();
        const area = n
          .closest(".conversation-attention")
          .getBoundingClientRect();
        return (
          box.top >= area.top - 1 &&
          box.bottom <= area.bottom + 1 &&
          box.bottom <= innerHeight
        );
      }),
      `${where}: the answer Tab reaches is brought into view`,
    );
    await area.evaluate((n) => n.scrollTo({ top: 0 }));
    await settled();
  };
  /** Waits for the inspector's (smooth) scroll to end: the same position twice running. */
  const inspectorSettled = async (where) => {
    let last;
    await until(async () => {
      const now = await inspector
        .locator(".inspector-body")
        .evaluate((n) => n.scrollTop);
      const still = now === last;
      last = now;
      return still;
    }, `${where}: the inspector never stopped scrolling`);
  };
  /**
   * Bringing a part of the inspector into view scrolls the inspector only: the window's
   * layout stays where it was (the conversation's top under the top bar, the inspector to
   * the window's bottom). Waits for a smooth scroll to end first.
   */
  const layoutInPlace = async (where) => {
    await inspectorSettled(where);
    const boxes = await page.evaluate(() => {
      const box = (selector) =>
        document.querySelector(selector)?.getBoundingClientRect();
      return {
        area: box(".work-area"),
        column: box(".content-column"),
        inspector: box("#twr-inspector"),
        height: innerHeight,
      };
    });
    assert(
      Math.abs(boxes.column.top - boxes.area.top) <= 1,
      `${where}: the conversation stays at the top of the work area (${boxes.column.top} vs ${boxes.area.top})`,
    );
    assert(
      Math.abs(boxes.inspector.bottom - boxes.height) <= 1,
      `${where}: the inspector reaches the window's bottom (${boxes.inspector.bottom} of ${boxes.height})`,
    );
  };

  // ---------------------------------------------------------------------------------
  // 1. A turn that ends with changes offers a quiet Review loop; offer_review = false
  //    turns it off. Not on the grid's tiles.
  // ---------------------------------------------------------------------------------
  await page.evaluate(async () => {
    await window.twr.rpc("thread.new", {
      project: "demo",
      name: "api",
      runtime: "fake",
      mode: "auto",
    });
    await window.twr.rpc("thread.send", {
      thread: "api",
      prompt: "!write lib.rs pub fn f() { todo!() }\nWrote it.",
    });
  });
  await until(() => {
    const t = thread("api");
    return t && t.status === "done" && Number(t.git?.dirty_files) > 0;
  }, "api's turn never ended with changes");
  await sidebarCard().waitFor();
  // The hub starts with `[loops] offer_review = false` (see startHub below): no button.
  await page.waitForTimeout(1000);
  assert.equal(
    await page.locator(".loop-offer").count(),
    0,
    "offer_review = false hides every Review loop button",
  );
  await talk();
  assert.equal(await page.locator(".loop-offer-row").count(), 0);
  await capture("00-offer-off", sidebarCard(), { narrow: false });
  // The same hub with the setting back on: the app reads it again on reconnecting.
  twr("hub", "stop");
  await new Promise((r) => hub.once("exit", r));
  writeFileSync(join(home, "config.toml"), config(true), { mode: 0o600 });
  await startHub();
  await connected();
  console.log(
    "PASS: [loops] offer_review = false hides the Review loop button",
  );
  const offer = sidebarCard().locator(".thread-card-offer");
  // The hub's shutdown stopped the thread, which goes on when it is sent something: it is
  // offered a review all the same.
  assert.equal(thread("api").stopped, true);
  await offer.waitFor({ timeout: 30000 });
  assert.equal(await offer.innerText(), "Review loop");
  assert.equal(await offer.getAttribute("aria-label"), "Review loop: api");
  // In the conversation, under the report.
  const conversationOffer = page.locator(".loop-offer-row");
  await conversationOffer.waitFor();
  assert.match(
    await conversationOffer.innerText(),
    /api's last turn left changes no loop has reviewed/,
  );
  await ariaOf("01-offer-card", sidebarCard());
  await ariaOf("01-offer-conversation", conversationOffer);
  await capture("01-offer", null, {
    narrow: () => compact("More"),
    restore: back,
  });
  // In the orchestrator's Latest thread reports.
  await toOrchestrator();
  const reports = page.locator("details").filter({
    hasText: "Latest thread reports",
  });
  await reports.locator("summary").click();
  await reports
    .locator(".report-row")
    .getByRole("button", { name: "Review loop: api" })
    .waitFor();
  await capture("02-offer-reports", reports, { narrow: false });
  await reports.locator("summary").click();
  // Not on the grid's tiles.
  await nav(/Thread grid/);
  const grid = page
    .locator(".manager")
    .filter({ has: page.getByRole("heading", { name: "Threads" }) });
  await grid.locator(".thread-card").filter({ hasText: "api" }).waitFor();
  assert.equal(
    await grid.locator(".loop-offer, .loop-chip").count(),
    0,
    "The grid's tiles have no loop controls",
  );
  await capture("03-grid-no-offer", grid, { narrow: () => compact("Threads") });
  console.log(
    "PASS: a turn with changes offers Review loop on its card, its conversation and the reports, not on grid tiles",
  );

  // ---------------------------------------------------------------------------------
  // 2. No saved settings: Review loop raises the setup card, inline and in Needs you,
  //    under the 600 ms rule.
  // ---------------------------------------------------------------------------------
  await toOrchestrator();
  // A click the moment the card appears answers nothing.
  await page.evaluate(() => {
    window.__earlyClick = new Promise((resolve) => {
      const tryClick = () => {
        const card = document.querySelector(
          "#twr-inspector .attention-loop-setup",
        );
        const start = [...(card?.querySelectorAll("button") || [])].find(
          (b) => b.textContent?.trim() === "Start and save",
        );
        if (!start) return false;
        const armed =
          card
            .querySelector(".attention-options")
            ?.getAttribute("data-armed") === "true";
        start.click();
        observer.disconnect();
        resolve(!armed);
        return true;
      };
      const observer = new MutationObserver(tryClick);
      observer.observe(document.body, { childList: true, subtree: true });
      tryClick();
    });
  });
  await sidebar();
  await offer.click();
  const inspectorCard = setupIn(inspector);
  await inspectorCard.waitFor({ timeout: 15000 });
  assert(
    await page.evaluate(() => window.__earlyClick),
    "The early click must land before the options arm",
  );
  await page.waitForTimeout(300);
  const first = await setupItem();
  assert(first, "The setup card is an open item");
  assert.equal(first.loop_setup, true);
  assert.equal(
    (await items()).filter((a) => a.attn_id === first.attn_id).length,
    1,
  );
  assert.equal(thread("api").loop, undefined, "Nothing started yet");
  console.log(
    "PASS: a click on Start and save before it armed answers nothing",
  );
  await inspectorCard
    .locator('.attention-options[data-armed="true"]')
    .waitFor({ timeout: 5000 });
  // The inspector brings the card into view and focuses it: its buttons are not below the
  // fold of an inspector that opened at its top.
  assert(
    await inspectorCard.evaluate((n) => n === document.activeElement),
    "The raised setup card has the focus",
  );
  // Measured once its smooth scroll has ended, never part-way.
  await inspectorSettled("The raised setup card");
  const startBox = await inspectorCard
    .getByRole("button", { name: "Start and save" })
    .boundingBox();
  assert(
    startBox && startBox.y + startBox.height <= defaultSize.height,
    "Start and save is on screen",
  );
  // Start and save reads in both themes (its text on its own background).
  for (const value of ["light", "dark"]) {
    await theme(value);
    const ratio = await contrast(
      inspectorCard.getByRole("button", { name: "Start and save" }),
    );
    assert(ratio >= 4.5, `Start and save's contrast in ${value}: ${ratio}`);
  }
  await theme("dark");
  const rows = await inspectorCard
    .locator(".loop-setup-rows dt")
    .allInnerTexts();
  for (const row of ["Profile", "Scope", "Implementer", "Reviewer"])
    assert(rows.includes(row), `The setup card has a ${row} row: ${rows}`);
  // The fake runtime runs no subagents: the hub leaves the Workers row out
  // (`settings::workers_apply`, `loop_setup.no_workers`).
  assert(
    !rows.includes("Workers"),
    `No Workers row for a fake thread: ${rows}`,
  );
  for (const name of ["Start and save", "Change…", "Cancel"])
    await inspectorCard.getByRole("button", { name, exact: true }).waitFor();
  assert.equal(
    await inspectorCard.getByRole("button", { name: "Dismiss" }).count(),
    0,
  );
  assert.match(
    await inspectorCard.locator(".loop-setup-title").innerText(),
    /^api: review loop setup/,
  );
  // Raising it said so, as other requests are announced.
  await announced("api asks you to choose a loop's settings");
  await ariaOf("04-setup-card", inspectorCard);
  await capture("04-setup-card", null, {
    narrow: () => compact("Needs you"),
    restore: select,
  });
  // In Needs you.
  await nav(/Needs you/);
  const needsCard = setupIn(page.locator(".manager"));
  await needsCard.waitFor();
  await ariaOf("05-setup-needs-you", needsCard);
  await capture("05-setup-needs-you", null, { narrow: false });
  // Inline in the thread's conversation.
  await talk();
  const inlineCard = setupIn(page.locator(".conversation-attention"));
  await inlineCard.waitFor();
  await inlineRequest(inlineCard, "The review loop's setup card");
  await capture("06-setup-inline", inlineCard, { narrow: false });
  console.log(
    "PASS: the setup card shows in the inspector, in Needs you and in the conversation, with its rows and three buttons",
  );

  // ---------------------------------------------------------------------------------
  // 3. Change… opens the editor; a change goes back to the card as a new revision.
  // ---------------------------------------------------------------------------------
  // The conversation's requests take at most 35% of it: in a short window the card's
  // answers are below its region's fold, and arm once scrolled into view, as for a person.
  await inlineCard.locator(".attention-options").scrollIntoViewIfNeeded();
  await inlineCard.locator('.attention-options[data-armed="true"]').waitFor();
  // With the keyboard: Tab from Start and save reaches Change…, Enter opens the editor.
  await inlineCard.getByRole("button", { name: "Start and save" }).focus();
  await page.keyboard.press("Tab");
  let now = await focused();
  assert.equal(now.text, "Change…");
  assert(now.visible, "Keyboard focus is visible on Change…");
  await page.keyboard.press("Tab");
  assert.equal((await focused()).text, "Cancel");
  await page.keyboard.press("Shift+Tab");
  await page.keyboard.press("Enter");
  const editor = page.getByRole("dialog", { name: "Loop settings · api" });
  await editor.waitFor();
  await editor.getByText("Loading loop settings…").waitFor({ state: "hidden" });
  const presets = editor.getByRole("group", { name: "Preset" });
  assert.equal(await presets.getByRole("button").count(), 4);
  const field = (name, group) =>
    (group ? editor.getByRole("group", { name: group }) : editor).getByRole(
      "combobox",
      { name, exact: true },
    );
  await field("Implementer").waitFor();
  // Named exactly "Implementer" (above); its hint describes it.
  assert.match(
    await field("Implementer").evaluate(
      (n) =>
        document.getElementById(n.getAttribute("aria-describedby"))
          ?.textContent || "",
    ),
    /^A named implementer runs on the thread's own runtime/,
  );
  await field("Runtime", "Reviewer").waitFor();
  await field("Model", "Reviewer").waitFor();
  await field("Profile").waitFor();
  await field("Review commits").waitFor();
  await editor.getByRole("group", { name: "Save for" }).waitFor();
  await ariaOf("07-editor", editor);
  await capture("07-editor", null);
  // Escape closes it and gives focus back to Change….
  await page.keyboard.press("Escape");
  await editor.waitFor({ state: "hidden" });
  now = await focused();
  assert.equal(now.text, "Change…", "Focus goes back to Change…");
  await page.keyboard.press("Enter");
  await editor.waitFor();
  await editor.getByText("Loading loop settings…").waitFor({ state: "hidden" });
  // A preset names the implementer: its model and effort pickers and its workers show.
  await presets.getByRole("button").first().click();
  assert.equal(
    await presets.getByRole("button").first().getAttribute("aria-pressed"),
    "true",
  );
  await field("Model", "Implementer").waitFor();
  await field("Effort", "Implementer").waitFor();
  await field("Model", "Workers").waitFor();
  await field("Effort", "Workers").waitFor();
  await ariaOf("08-editor-preset", editor);
  await capture("08-editor-preset", null);
  // Back to the thread's own model (a fake thread), and the low profile.
  await field("Implementer").selectOption("");
  await field("Profile").selectOption("low");
  const shown = (await setupItem()).revision;
  await editor.getByRole("button", { name: "Save to the card" }).click();
  await editor.waitFor({ state: "hidden" });
  await until(
    async () => (await setupItem())?.revision > shown,
    "The card was not raised again with a new revision",
  );
  await inlineCard
    .locator(".loop-setup-rows dd")
    .first()
    .filter({ hasText: /^low/ })
    .waitFor({ timeout: 10000 });
  await announced("Setup card updated");
  await capture("09-setup-changed", inlineCard, { narrow: false });
  console.log(
    "PASS: Change… opens the editor (presets, pickers, profile, commits, Save for) and its change raises the card again",
  );

  // ---------------------------------------------------------------------------------
  // 4. Start and save: the loop runs, with its chip, its state and its progress.
  // ---------------------------------------------------------------------------------
  hold(1);
  hold(2);
  // The raised card arms again from zero.
  assert.equal(
    await inlineCard.locator(".attention-options").getAttribute("data-armed"),
    "false",
  );
  await inlineCard.locator(".attention-options").scrollIntoViewIfNeeded();
  await inlineCard.locator('.attention-options[data-armed="true"]').waitFor();
  await inlineCard.getByRole("button", { name: "Start and save" }).click();
  await until(() => runs() === 1, "The first review run never started");
  await until(
    () => thread("api").status === "background",
    "The thread is not background while the reviewer runs",
  );
  const running = thread("api");
  assert.equal(running.loop.waiting_on, "review");
  await sidebar();
  const chip = sidebarCard().locator(".loop-chip");
  await chip.waitFor({ timeout: 15000 });
  assert.equal(await chip.locator(".loop-badge").innerText(), "REVIEW LOOP");
  assert.match(await chip.locator(".loop-line").innerText(), /^low · /);
  const cardText = await sidebarCard().locator(".thread-card").innerText();
  assert.match(cardText, /background/);
  assert.match(
    cardText,
    /gpt-6\.1-sol/,
    "The card's activity names the reviewer",
  );
  assert.equal(
    await sidebarCard().locator(".loop-offer").count(),
    0,
    "No Review loop offer while a loop runs",
  );
  const saved = twrJson("loop", "settings");
  assert.equal(saved.settings.profile, "low");
  await ariaOf("10-running-card", sidebarCard());
  // A click on the chip opens the inspector at its Loops section.
  await toOrchestrator();
  await chip.click();
  await loopsSection.waitFor();
  assert(
    await loopsSection.evaluate((n) => n === document.activeElement),
    "The chip's click focuses the Loops section",
  );
  await layoutInPlace("The chip's Loops section");
  const status = loopsSection.locator(".loop-status");
  await status.filter({ hasText: "waiting on the review" }).waitFor();
  assert.match(await status.innerText(), /REVIEW LOOP\s+low/);
  assert.equal(
    await loopsSection
      .locator('.loop-phases [aria-current="step"]')
      .innerText(),
    "ladder",
  );
  await loopsSection
    .locator(".loop-facts dd")
    .filter({ hasText: "(settings chosen on its setup card)" })
    .waitFor();
  // The round in flight is under review, not a round with no findings.
  await loopsSection
    .getByRole("list", { name: "Rounds" })
    .getByText("m1 under review")
    .waitFor();
  const controls = loopsSection.getByRole("group", { name: "Loop controls" });
  for (const name of ["Pause", "Skip to verify", "Stop…"])
    await controls.getByRole("button", { name, exact: true }).waitFor();
  await ariaOf("11-inspector-running", loopsSection);
  await capture("11-inspector-running", null, {
    narrow: inspectorNarrow(loopsSection),
    restore: back,
  });
  // With the keyboard only: from the focused section, Tab reaches its controls in order,
  // with a visible focus; Stop… asks first, and Escape gives the focus back.
  await loopsSection.focus();
  await page.keyboard.press("Tab");
  now = await focused();
  assert.equal(now.text, "Pause");
  assert(now.visible, "Keyboard focus is visible on Pause");
  await page.keyboard.press("Tab");
  assert.equal((await focused()).text, "Skip to verify");
  await page.keyboard.press("Tab");
  assert.equal((await focused()).text, "Stop…");
  await page.keyboard.press("Enter");
  const stopDialog = page.getByRole("dialog", {
    name: "Stop api's review loop?",
  });
  await stopDialog.waitFor();
  await ariaOf("12-stop-confirm", stopDialog);
  await page.keyboard.press("Escape");
  await stopDialog.waitFor({ state: "hidden" });
  assert.equal((await focused()).text, "Stop…", "Focus goes back to Stop…");
  assert(thread("api").loop, "Escape stops nothing");
  await page.keyboard.press("Shift+Tab");
  await page.keyboard.press("Shift+Tab");
  assert.equal((await focused()).text, "Pause");
  // Pause and resume, from the keyboard.
  await page.keyboard.press("Enter");
  await controls.getByRole("button", { name: "Resume" }).waitFor();
  await announced("Loop paused");
  assert.equal(twrJson("loop", "list")[0].paused, true);
  await status.filter({ hasText: "paused" }).waitFor();
  await capture("12-inspector-paused", loopsSection, { narrow: false });
  await controls.getByRole("button", { name: "Resume" }).focus();
  await page.keyboard.press(" ");
  await controls.getByRole("button", { name: "Pause" }).waitFor();
  await announced("Loop resumed");
  assert.equal(twrJson("loop", "list")[0].paused, false);
  console.log(
    "PASS: Start and save starts the loop: chip, background state naming the reviewer, inspector progress, pause and resume",
  );
  // Open report while it runs: the dialog says the report is not written yet, and where
  // the hub will write it (`loop.report`); Close gives the focus back to Open report.
  await loopsSection.getByRole("button", { name: "Open report" }).click();
  const notYet = page.getByRole("dialog", { name: "Review loop report · api" });
  await notYet
    .locator(".loop-report-body")
    .getByText(
      "api's review loop is still running: its report is not written yet (the hub writes it when the loop ends)",
    )
    .waitFor({ timeout: 15000 });
  assert.equal(
    await notYet.locator(".loop-report-body .markdown").count(),
    0,
    "No text for a report not written",
  );
  assert.match(await notYet.innerText(), /\brunning\b/);
  const futurePath = await notYet
    .locator(".loop-report-footer .loop-path code")
    .innerText();
  assert.match(futurePath, /report\.md$/);
  assert(!existsSync(futurePath), "The running loop's report is not written");
  assert.match(
    await notYet.locator(".loop-report-footer").textContent(),
    /To be written on the hub's machine at/,
  );
  await ariaOf("25-report-running", notYet);
  await capture("25-report-running", null);
  await notYet.getByRole("button", { name: "Close dialog" }).click();
  await notYet.waitFor({ state: "hidden" });
  assert.equal(
    (await focused()).text,
    "Open report",
    "Focus goes back to Open report",
  );
  assert(thread("api").loop, "Reading the report changes nothing");
  console.log(
    "PASS: Open report on a running loop says its report is not written yet, and where it will be",
  );
  // The first round answers one finding; the thread dismisses it; the verification run
  // is held: the progress shows the round with its finding and the ledger.
  release(1);
  await until(() => runs() === 2, "The verification run never started");
  const rounds = loopsSection.getByRole("list", { name: "Rounds" });
  await rounds
    .getByText("1 finding", { exact: false })
    .waitFor({ timeout: 15000 });
  await loopsSection
    .locator(".loop-facts dd")
    .filter({ hasText: "1 entry" })
    .waitFor();
  await rounds.locator("summary").first().click();
  await rounds.getByText("it breaks on empty input").waitFor();
  await rounds.getByText(/dismissed: empty input never reaches it/).waitFor();
  await ariaOf("13-inspector-rounds", loopsSection);
  await capture("13-inspector-rounds", null, {
    narrow: inspectorNarrow(loopsSection.getByRole("list", { name: "Rounds" })),
    restore: back,
  });
  console.log(
    "PASS: the inspector shows the round, its finding, the verdict and the ledger",
  );

  // ---------------------------------------------------------------------------------
  // 5. The loop ends: its report is the thread's report, the chip goes, the offer stays
  //    away for the changes it reviewed.
  // ---------------------------------------------------------------------------------
  release(2);
  await until(() => {
    const t = thread("api");
    return !t.loop && t.last_loop && t.status !== "background";
  }, "The loop never ended");
  const ended = thread("api");
  assert.equal(ended.last_loop.outcome, "clean");
  await until(
    () =>
      /review loop clean after 2 rounds/.test(
        thread("api").last_report?.summary || "",
      ),
    "The loop's report is not the thread's report",
  );
  await chip.waitFor({ state: "hidden", timeout: 15000 });
  await announced("api's review loop ended: clean");
  await inspector
    .locator(".inspector-report")
    .getByText(/review loop clean after 2 rounds/)
    .waitFor({ timeout: 15000 });
  await loopsSection.getByText("Last review loop: clean").waitFor();
  const openReport = loopsSection.getByRole("button", { name: "Open report" });
  await openReport.click();
  // The report's text in a dialog (`loop.report`), the path under it.
  const reportDialog = page.getByRole("dialog");
  await reportDialog
    .locator(".loop-report-body")
    .getByText(/review loop/i)
    .first()
    .waitFor({ timeout: 15000 });
  const path = await reportDialog
    .locator(".loop-report-footer .loop-path code")
    .first()
    .innerText();
  assert.match(path, /report\.md$/);
  assert(existsSync(path), "The report's path exists");
  // The dialog shows the report's text as the hub wrote it, in a region that scrolls.
  assert.equal(
    await reportDialog
      .getAttribute("aria-labelledby")
      .then((id) =>
        page.evaluate((id) => document.getElementById(id)?.textContent, id),
      ),
    "Review loop report · api",
  );
  assert.match(await reportDialog.innerText(), /\bclean\b/);
  const reportRegion = reportDialog.getByRole("region", { name: "Report" });
  await reportRegion
    .getByText("The loop reviewed the change; nothing is left open.")
    .waitFor();
  const onDisk = readFileSync(path, "utf8");
  assert(
    onDisk.includes("The loop reviewed the change; nothing is left open."),
    "The dialog's text is report.md's",
  );
  assert.equal(
    await reportRegion.evaluate((n) => getComputedStyle(n).overflowY),
    "auto",
    "The report scrolls inside its dialog",
  );
  await ariaOf("26-report-finished", reportDialog);
  await capture("26-report-finished", null);
  await reportDialog.getByRole("button", { name: "Close dialog" }).click();
  await reportDialog.waitFor({ state: "hidden", timeout: 15000 });
  await page.waitForTimeout(1500);
  assert.equal(
    await page.locator(".loop-offer").count(),
    0,
    "No Review loop offer for changes a loop reviewed",
  );
  await ariaOf("14-ended", inspector);
  await capture("14-ended", null, {
    narrow: inspectorNarrow(loopsSection),
    restore: back,
  });
  console.log(
    "PASS: the loop ends clean: the thread's report says so, the chip goes, Open report shows report.md, no offer comes back",
  );

  // ---------------------------------------------------------------------------------
  // 6. Saved settings: /review starts at once and says what it runs on; /loop controls it;
  //    a stuck loop raises its item with resume and stop.
  // ---------------------------------------------------------------------------------
  hold(3);
  await talk();
  const composer = page.getByRole("textbox", { name: "Message api directly" });
  await composer.fill("/review");
  await composer.press("Enter");
  await until(() => thread("api").loop, "/review did not start a loop");
  assert.equal(await setupItem(), undefined, "No setup card the second time");
  const toast = page.locator(".notice-toast");
  // It says what it runs on, and whose settings they are.
  await toast
    .filter({
      hasText:
        "api started a review loop: low · implementer fake-1 (the thread's own) · reviewer codex gpt-6.1-sol (saved settings).",
    })
    .waitFor();
  assert.equal(await composer.inputValue(), "", "/review clears the composer");
  await capture("15-second-run", null, { narrow: false });
  await until(() => runs() === 3, "The second loop's run never started");
  await composer.fill("/loop pause");
  await composer.press("Enter");
  await until(
    () => twrJson("loop", "list")[0]?.paused,
    "/loop pause did not pause",
  );
  await composer.fill("/loop resume");
  await composer.press("Enter");
  await until(
    () => twrJson("loop", "list")[0]?.paused === false,
    "/loop resume did not resume",
  );
  console.log(
    "PASS: /review starts on the saved settings; /loop pause and resume control it",
  );
  // The run fails, and its retry too: the loop is stuck. The window is hidden: the stuck
  // loop notifies, saying what it is and not what failed.
  failRun(3);
  failRun(4);
  const stuckItem = await hidden(async () => {
    await page.waitForTimeout(1500);
    release(3);
    const item = await until(
      async () => (await items()).find((a) => a.loop_stuck),
      "No stuck item after two failed runs",
    );
    const notice = await notified("Has a loop that is stuck");
    assert.equal(notice.title, "api · default");
    return item;
  });
  console.log("PASS: a stuck loop notifies as one, without what failed");
  await select();
  const stuck = inspector
    .locator(".attention-card")
    .filter({ hasText: "Loop stuck" });
  await stuck.waitFor();
  for (const name of ["Resume", "Stop"])
    await stuck.getByRole("button", { name, exact: true }).waitFor();
  assert.equal(await stuck.getByRole("button", { name: "Dismiss" }).count(), 0);
  await ariaOf("16-stuck", stuck);
  await layoutInPlace("The stuck loop's inspector");
  await capture("16-stuck", null, {
    narrow: () => compact("Needs you"),
    restore: select,
  });
  await stuck.scrollIntoViewIfNeeded();
  await stuck.locator('.attention-options[data-armed="true"]').waitFor();
  await stuck.getByRole("button", { name: "Resume", exact: true }).click();
  await until(() => runs() >= 5, "Resume did not ask for the run again");
  await until(
    async () => !(await items()).some((a) => a.attn_id === stuckItem.attn_id),
    "The stuck item stays after resume",
  );
  await until(() => !thread("api").loop, "The resumed loop never ended");
  console.log(
    "PASS: a stuck loop raises its item with resume and stop; resume goes on",
  );
  // /review --setup shows the card on the saved settings; while the window is hidden it
  // notifies as a request for the loop's settings. Cancel starts and saves nothing.
  // The notification is waited for while the window is still hidden: shown again at once,
  // it could come forward before the card reached the app, which then posts none.
  await hidden(async () => {
    await page.waitForTimeout(1500);
    try {
      twr("loop", "review", "api", "--setup", "--json");
    } catch {
      // Exit 2: the card is left open for the app to answer.
    }
    await notified("Asks you to choose a loop's settings");
  });
  await talk();
  const again = setupIn(page.locator(".conversation-attention"));
  await again.waitFor();
  assert.match(
    await again.locator(".loop-setup-title").innerText(),
    /^api: review loop setup/,
  );
  await again.locator(".attention-options").scrollIntoViewIfNeeded();
  await again.locator('.attention-options[data-armed="true"]').waitFor();
  await again.getByRole("button", { name: "Cancel", exact: true }).click();
  await again.waitFor({ state: "hidden", timeout: 15000 });
  assert.equal(await setupItem(), undefined);
  assert.equal(thread("api").loop, undefined, "Cancel starts nothing");
  assert.equal(twrJson("loop", "settings").settings.profile, "low");
  console.log(
    "PASS: a setup card notifies while hidden; Cancel starts and saves nothing",
  );

  // ---------------------------------------------------------------------------------
  // 7. Settings › Loops: the same editor outside a loop; one project's own settings.
  // ---------------------------------------------------------------------------------
  await composer.fill("/loop settings --project demo");
  await composer.press("Enter");
  const projectEditor = page.getByRole("dialog", {
    name: "Loop settings · demo",
  });
  await projectEditor.waitFor();
  await projectEditor
    .getByText("Loading loop settings…")
    .waitFor({ state: "hidden" });
  await projectEditor
    .getByRole("group", { name: "Save for" })
    .getByRole("button", { name: "demo only" })
    .click();
  // Saved settings name no runtime for the workers: they still show as saved.
  const workersEffort = projectEditor
    .getByRole("group", { name: "Workers" })
    .getByRole("combobox", { name: "Effort" });
  assert.equal(await workersEffort.inputValue(), "high");
  await projectEditor
    .getByRole("combobox", { name: "Profile", exact: true })
    .selectOption("medium");
  await projectEditor.getByRole("button", { name: "Save settings" }).click();
  await projectEditor
    .getByText("Saved. Loops start on these settings")
    .waitFor();
  await capture("17-project-saved", projectEditor, { narrow: false });
  await projectEditor
    .getByRole("button", { name: "Close", exact: true })
    .last()
    .click();
  assert.equal(
    twrJson("loop", "settings", "--project", "demo").source,
    "project",
  );
  assert.equal(
    twrJson("loop", "settings", "--project", "demo").settings.profile,
    "medium",
  );
  assert.equal(twrJson("loop", "settings").settings.profile, "low");
  await sidebar();
  await page
    .locator(".sidebar")
    .getByRole("button", { name: "Settings", exact: true })
    .first()
    .click();
  const panel = page
    .locator(".settings-section")
    .filter({ has: page.getByRole("heading", { name: "Loops", exact: true }) });
  await panel.waitFor();
  const projects = panel.getByRole("list", {
    name: "Projects with their own settings",
  });
  await projects.getByText("demo", { exact: true }).waitFor();
  await panel.scrollIntoViewIfNeeded();
  await ariaOf("18-settings-loops", panel);
  await capture("18-settings-loops", null, {
    narrow: () => panel.scrollIntoViewIfNeeded(),
  });
  await projects.getByRole("button", { name: "Open" }).click();
  await panel.getByRole("heading", { name: "demo's settings" }).waitFor();
  await panel
    .getByRole("combobox", { name: "Profile", exact: true })
    .selectOption("high");
  await panel.getByRole("button", { name: "Save settings" }).click();
  await until(
    () =>
      twrJson("loop", "settings", "--project", "demo").settings.profile ===
      "high",
    "Saving for demo from Settings › Loops did not save",
  );
  assert.equal(twrJson("loop", "settings").settings.profile, "low");
  await capture("19-settings-project", panel, { narrow: false });
  console.log(
    "PASS: Settings › Loops shows the editor and the projects with their own settings; saving for one project works",
  );

  // ---------------------------------------------------------------------------------
  // 8. Implementation loops. With no saved settings, Send as implementation loop raises
  //    the setup card holding the task; Run without a loop sends it as an ordinary
  //    message. Again with Start and save: plan, implement, a round, the report and the
  //    commit. The New thread form's switch starts one on saved settings.
  // ---------------------------------------------------------------------------------
  await page.evaluate(async () => {
    await window.twr.rpc("loop.settings.reset", {});
    await window.twr.rpc("thread.new", {
      project: "demo",
      name: "web",
      runtime: "fake",
      mode: "auto",
    });
  });
  assert.equal(twrJson("loop", "settings").settings ?? null, null);
  const webCard = () =>
    page.locator(".thread-list .thread-item").filter({ hasText: "web" });
  await sidebar();
  await webCard().waitFor();
  await webCard().locator(".thread-card").click();
  await page.getByRole("button", { name: "Talk to this thread" }).click();
  const webBox = page.getByRole("textbox", { name: "Message web directly" });
  await webBox.waitFor();
  await page.getByText("Loading conversation…").waitFor({ state: "hidden" });
  const task = "add a CSV export to the reports page";
  /** Types the task and picks Send as implementation loop from Send's caret. */
  const sendAsLoop = async () => {
    await webBox.fill(task);
    const caret = page.getByRole("button", { name: "More ways to send" });
    assert.equal(await caret.getAttribute("aria-expanded"), "false");
    await caret.click();
    assert.equal(await caret.getAttribute("aria-expanded"), "true");
    const choice = page.getByRole("button", {
      name: "Send as implementation loop",
      exact: true,
    });
    await choice.waitFor();
    return choice;
  };
  // The choice is keyboard-reachable and Escape closes it, giving focus back to the caret.
  await sendAsLoop();
  assert.equal((await focused()).text, "Send as implementation loop");
  await page.keyboard.press("Escape");
  assert.equal((await focused()).text, "More ways to send");
  await sendAsLoop();
  await ariaOf("20-send-as-loop", page.locator(".composer"));
  await capture("20-send-as-loop", null);
  const sendCaret = page.getByRole("button", { name: "More ways to send" });
  if ((await sendCaret.getAttribute("aria-expanded")) === "true")
    await sendCaret.click();
  (await sendAsLoop()).click();
  const webSetup = setupIn(page.locator(".conversation-attention"));
  await webSetup.waitFor({ timeout: 15000 });
  assert.equal(await webBox.inputValue(), "", "The task left the composer");
  assert.match(
    await webSetup.locator(".loop-setup-title").innerText(),
    /^web: implementation loop setup/,
  );
  assert.match(
    await webSetup.locator(".attention-kind").innerText(),
    /Implementation loop setup/,
  );
  const webRows = await webSetup.locator(".loop-setup-rows dt").allInnerTexts();
  for (const row of ["Task", "Plan check", "Commit"])
    assert(webRows.includes(row), `The card has a ${row} row: ${webRows}`);
  assert.equal(
    await webSetup.getByRole("region", { name: "Task" }).innerText(),
    task,
  );
  await inlineRequest(webSetup, "The implementation loop's setup card");
  // The conversation's requests take at most a third of it: the card's answers arm once
  // scrolled into view, as for a person.
  await webSetup.locator(".attention-options").scrollIntoViewIfNeeded();
  await webSetup.locator('.attention-options[data-armed="true"]').waitFor();
  assert.deepEqual(
    await webSetup.locator(".attention-options button").allInnerTexts(),
    ["Start and save", "Change…", "Run without a loop", "Cancel"],
  );
  await dismissNotices();
  await ariaOf("21-setup-task", webSetup);
  await capture("21-setup-task", null, {
    // Resized, the answers arm again from zero: shown armed.
    narrow: async () => {
      await webSetup.locator(".attention-options").scrollIntoViewIfNeeded();
      await webSetup
        .locator('.attention-options[data-armed="true"]')
        .waitFor({ timeout: 5000 });
    },
    restore: () =>
      webSetup.locator(".attention-options").scrollIntoViewIfNeeded(),
  });
  // In Needs you too, with the same three answers.
  await nav(/Needs you/);
  const needsTask = setupIn(page.locator(".manager"));
  await needsTask.waitFor();
  await needsTask.getByRole("button", { name: "Run without a loop" }).waitFor();
  await sidebar();
  await webCard().locator(".thread-card").dblclick();
  await webSetup.waitFor();
  // The conversation's requests take at most a third of it: the card's answers arm once
  // scrolled into view, as for a person.
  await webSetup.locator(".attention-options").scrollIntoViewIfNeeded();
  await webSetup.locator('.attention-options[data-armed="true"]').waitFor();
  await webSetup.getByRole("button", { name: "Run without a loop" }).click();
  await webSetup.waitFor({ state: "hidden", timeout: 15000 });
  await until(async () => {
    const events = await page.evaluate(() =>
      window.twr.rpc("thread.transcript", { thread: "web" }),
    );
    return (events.events || events).some(
      (e) =>
        e.kind === "agent.text" && e.data?.final && e.data.text?.includes(task),
    );
  }, "Run without a loop did not send the task as an ordinary message");
  assert.equal(thread("web").loop, undefined, "Run without a loop starts none");
  assert.equal(
    twrJson("loop", "settings").settings ?? null,
    null,
    "Run without a loop saves nothing",
  );
  console.log(
    "PASS: Send as implementation loop raises the card holding the task; Run without a loop sends it as a message",
  );

  // Again, with Start and save: the loop runs. Its plan and implement turns take a while
  // (the fake thread sleeps first), to look at the inspector during each; its first review
  // run is held.
  const firstRun = runs() + 1;
  hold(firstRun);
  writeFileSync(join(templates, "plan.md"), "!sleep 15000\n" + PLAN);
  writeFileSync(join(templates, "implement.md"), "!sleep 15000\n" + IMPLEMENT);
  await until(() => thread("web").status !== "running", "web is still busy");
  (await sendAsLoop()).click();
  await webSetup.waitFor({ timeout: 15000 });
  // The conversation's requests take at most a third of it: the card's answers arm once
  // scrolled into view, as for a person.
  await webSetup.locator(".attention-options").scrollIntoViewIfNeeded();
  await webSetup.locator('.attention-options[data-armed="true"]').waitFor();
  await webSetup.getByRole("button", { name: "Start and save" }).click();
  const webLoops = inspector.locator(".inspector-loops");
  /** web's card selected: its inspector. */
  const selectWeb = async () => {
    await sidebar();
    await webCard().locator(".thread-card").click();
    await inspector.waitFor();
  };
  const current = () =>
    webLoops.locator('.loop-phases [aria-current="step"]').innerText();
  await until(
    () => thread("web").loop?.phase === "plan",
    "The implementation loop never planned",
  );
  await selectWeb();
  await until(
    async () => (await current()) === "plan",
    "The plan is not current",
  );
  await webLoops
    .locator(".loop-status")
    .filter({ hasText: "IMPLEMENT LOOP" })
    .waitFor();
  await ariaOf("27-implement-plan", webLoops);
  await capture("27-implement-plan", null, {
    narrow: inspectorNarrow(webLoops),
    restore: selectWeb,
  });
  console.log("PASS: the inspector shows an implementation loop planning");
  await until(
    () => thread("web").loop?.phase === "implement",
    "The implementation loop never implemented",
    40000,
  );
  // The implement prompt is out: the templates go back to answering at once.
  writeFileSync(join(templates, "plan.md"), PLAN);
  writeFileSync(join(templates, "implement.md"), IMPLEMENT);
  await selectWeb();
  await until(
    async () => (await current()) === "implement",
    "Implement is not current",
  );
  await webLoops
    .locator(".loop-implement dd")
    .filter({ hasText: "revision 1 · contract risk no: UI only: a new button" })
    .waitFor();
  await ariaOf("28-implement-implementing", webLoops);
  await capture("28-implement-implementing", null, {
    narrow: inspectorNarrow(webLoops),
    restore: selectWeb,
  });
  console.log(
    "PASS: the inspector shows the plan's revision and contract risk while the thread implements",
  );
  await until(
    () => runs() >= firstRun,
    "The ladder's first run never started",
    40000,
  );
  await selectWeb();
  await webLoops
    .locator(".loop-status")
    .filter({ hasText: "IMPLEMENT LOOP" })
    .waitFor();
  const phaseWords = await webLoops.locator(".loop-phases li").allInnerTexts();
  assert.deepEqual(
    phaseWords.map((w) =>
      w
        .replace(/\s*\(done\)/, "")
        .replace(/\s+/g, " ")
        .trim(),
    ),
    [
      "plan",
      "plan check skipped",
      "implement",
      "ladder",
      "sweep",
      "verify",
      "report",
    ],
    "The phases, the plan check skipped",
  );
  await webLoops
    .locator(".loop-implement dd")
    .filter({ hasText: "revision 1 · contract risk no: UI only: a new button" })
    .waitFor();
  await webLoops
    .locator(".loop-implement dd")
    .filter({ hasText: "done · gates pass: true" })
    .waitFor();
  await webLoops
    .locator(".loop-implement code")
    .filter({ hasText: /plan\.md$/ })
    .waitFor();
  await sidebar();
  const webChip = webCard().locator(".loop-chip");
  await webChip.waitFor();
  assert.equal(
    await webChip.locator(".loop-badge").innerText(),
    "IMPLEMENT LOOP",
  );
  await ariaOf("22-implement-running", webLoops);
  await capture("22-implement-running", null, {
    narrow: inspectorNarrow(webLoops),
    restore: selectWeb,
  });
  console.log(
    "PASS: Start and save starts the implementation loop: its chip, phases, plan and implement result",
  );
  // The rest runs with the window hidden: the end notifies once, with the commit.
  await hidden(async () => {
    await page.waitForTimeout(1500);
    release(firstRun);
    await until(
      () => thread("web").loop?.phase === "report",
      "The loop never reached its report",
      60000,
    );
    const wt = thread("web").worktree;
    execFileSync("git", ["-C", wt, "add", "-A"], { stdio: "ignore" });
    execFileSync(
      "git",
      [
        "-C",
        wt,
        "-c",
        "user.name=Desktop Test",
        "-c",
        "user.email=desktop@example.invalid",
        "commit",
        "-q",
        "-m",
        "Add the CSV export",
      ],
      { stdio: "ignore" },
    );
    await until(
      () => !thread("web").loop && thread("web").last_loop,
      "The implementation loop never ended",
      60000,
    );
    const last = thread("web").last_loop;
    assert.equal(last.kind, "implement");
    assert.equal(last.outcome, "clean");
    assert.equal(last.commits?.length, 1, JSON.stringify(last));
    const sha = last.commits[0].sha.slice(0, 7);
    const notice = await until(async () => {
      await send({ type: "inbox-poll" });
      return (await notifications()).find((n) =>
        /^Implementation loop clean/.test(n.body),
      );
    }, "No notification of the loop's end");
    assert.equal(notice.title, "web · default");
    assert.match(
      notice.body,
      new RegExp(
        `^Implementation loop clean after \\d+ rounds? · committed ${sha}$`,
      ),
    );
    // Once: later summaries list the same end, and it is not news again.
    for (let i = 0; i < 3; i++) {
      await send({ type: "inbox-poll" });
      await page.waitForTimeout(700);
    }
    assert.equal(
      (await notifications()).filter(
        (n) =>
          n.title === "web · default" && /^Implementation loop/.test(n.body),
      ).length,
      1,
      "The loop's end notifies once",
    );
  });
  const sha = thread("web").last_loop.commits[0].sha.slice(0, 7);
  await announced(`web's implementation loop ended: clean; committed ${sha}`);
  await select();
  await webCard().locator(".thread-card").click();
  await webLoops.getByText("Last implementation loop: clean").waitFor();
  await webLoops
    .locator(".loop-commits code")
    .filter({ hasText: sha })
    .waitFor();
  await sidebar();
  await webCard()
    .locator(".loop-ended")
    .filter({ hasText: `committed ${sha}` })
    .waitFor();
  await ariaOf("23-implement-ended", webLoops);
  await capture("23-implement-ended", null, {
    narrow: inspectorNarrow(webLoops),
    restore: selectWeb,
  });
  console.log(
    "PASS: the loop plans, implements, reviews, reports and commits; the commit shows and notifies once",
  );

  // The inspector's Implementation loop… form fits the window: its task and its Start in
  // view. Cancel starts nothing and gives the focus back to Implementation loop….
  await selectWeb();
  await webLoops.getByRole("button", { name: "Implementation loop…" }).click();
  const implementForm = page.getByRole("dialog", {
    name: "Implementation loop · web",
  });
  await implementForm.waitFor();
  const windowHeight = await page.evaluate(() => innerHeight);
  for (const [name, control] of [
    ["Its Task field", implementForm.getByRole("textbox", { name: "Task" })],
    [
      "Start implementation loop",
      implementForm.getByRole("button", { name: "Start implementation loop" }),
    ],
  ]) {
    const box = await control.boundingBox();
    assert(
      box && box.y >= 0 && box.y + box.height <= windowHeight,
      `${name} is on screen in the Implementation loop… form`,
    );
  }
  await ariaOf("34-implement-form", implementForm);
  await capture("34-implement-form", null, { narrow: false });
  await implementForm
    .getByRole("button", { name: "Cancel", exact: true })
    .click();
  await implementForm.waitFor({ state: "hidden" });
  assert.equal((await focused()).text, "Implementation loop…");
  assert.equal(thread("web").loop, undefined, "Cancel starts nothing");
  console.log(
    "PASS: the inspector's Implementation loop… form fits the window; Cancel starts nothing",
  );

  // The New thread form's switch: on saved settings the loop starts at once. Its report
  // commits nothing (the script does not commit this time): it ends `not committed`.
  await sidebar();
  await page
    .getByRole("button", { name: "New thread", exact: true })
    .first()
    .click();
  const form = page.getByRole("dialog", { name: "New thread" });
  await form.waitFor();
  await form
    .getByRole("textbox", { name: /Project or repository/ })
    .fill("demo");
  await form.getByRole("textbox", { name: "Name" }).fill("csv");
  await form.getByRole("textbox", { name: "First prompt" }).fill(task);
  const loopSwitch = form.getByRole("switch", {
    name: "Run as an implementation loop",
  });
  await loopSwitch.check();
  await form
    .locator(".new-thread-loop .loop-settings-line")
    .filter({ hasText: "saved settings" })
    .waitFor();
  await form
    .getByRole("button", { name: "Change the loop settings…" })
    .waitFor();
  await ariaOf("24-new-thread-loop", form);
  await capture("24-new-thread-loop", null);
  await form.getByRole("button", { name: "Create thread" }).click();
  await until(() => thread("csv"), "The New thread form made no thread");
  await until(
    () => thread("csv").loop || thread("csv").last_loop,
    "The New thread form's switch started no loop",
    30000,
  );
  // It ends with the window hidden: its notification says it was not committed.
  await hidden(async () => {
    await until(() => thread("csv").last_loop, "csv's loop never ended", 90000);
    assert(
      thread("csv").last_loop.commit_problem,
      "Nothing committed: the loop says why",
    );
    const notice = await until(async () => {
      await send({ type: "inbox-poll" });
      return (await notifications()).find((n) => n.title === "csv · default");
    }, "No notification of csv's loop's end");
    assert.match(notice.body, /^Implementation loop .+ · not committed: /);
    await send({ type: "inbox-poll" });
    await page.waitForTimeout(1000);
    assert.equal(
      (await notifications()).filter((n) => n.title === "csv · default").length,
      1,
      "csv's end notifies once",
    );
  });
  await sidebar();
  await page
    .locator(".thread-list .thread-item")
    .filter({ hasText: "csv" })
    .locator(".loop-ended")
    .filter({ hasText: "not committed" })
    .waitFor({ timeout: 15000 });
  console.log(
    "PASS: the New thread form's switch starts an implementation loop with the first prompt as its task; an uncommitted end says so",
  );

  // ---------------------------------------------------------------------------------
  // 9. /implement: in a thread's composer every word is the task, and it starts at once on
  //    the saved settings; in the orchestrator's the first word names the thread.
  // ---------------------------------------------------------------------------------
  /** The task the hub holds for `name`'s running loop (`task.md` in its folder). */
  const heldTask = async (name) => {
    const detail = await page.evaluate(
      (name) => window.twr.rpc("loop.get", { thread: name }),
      name,
    );
    try {
      return readFileSync(join(detail.folder, "task.md"), "utf8");
    } catch {
      return JSON.stringify(detail);
    }
  };
  /** Stops `name`'s loop from the command line and waits for its end. */
  const stopLoop = async (name) => {
    twr("loop", "stop", name, "--yes");
    await until(() => !thread(name).loop, `${name}'s loop did not stop`);
  };
  await until(
    () => !["running", "background"].includes(thread("web").status),
    "web is still busy",
  );
  await selectWeb();
  await page.getByRole("button", { name: "Talk to this thread" }).click();
  await webBox.waitFor();
  const jsonTask = "add a JSON export to the reports page";
  await webBox.fill(`/implement ${jsonTask}`);
  await webBox.press("Enter");
  await until(
    () => thread("web").loop?.kind === "implement",
    "/implement in web's composer started no implementation loop",
  );
  assert.equal(await setupItem(), undefined, "Saved settings: no setup card");
  await toast
    .filter({
      hasText: /^web started an implementation loop: .+ \(saved settings\)\.$/,
    })
    .waitFor();
  assert.equal(await webBox.inputValue(), "", "/implement clears the composer");
  assert.match(await heldTask("web"), new RegExp(jsonTask));
  await ariaOf("29-implement-slash-thread", page.locator(".composer"));
  await capture("29-implement-slash-thread", null);
  await stopLoop("web");
  console.log(
    "PASS: /implement in a thread's composer starts its loop at once with the words as the task",
  );
  await toOrchestrator();
  const orchestratorBox = page.getByRole("textbox", {
    name: "Message orchestrator",
  });
  await orchestratorBox.waitFor();
  const pdfTask = "add a PDF export";
  await orchestratorBox.fill(`/implement csv ${pdfTask}`);
  await orchestratorBox.press("Enter");
  await until(
    () => thread("csv").loop?.kind === "implement",
    "/implement csv … in the orchestrator's composer started no loop in csv",
  );
  assert.equal(thread("web").loop, undefined, "Only the named thread runs it");
  await toast
    .filter({ hasText: /^csv started an implementation loop: / })
    .waitFor();
  assert.equal(await orchestratorBox.inputValue(), "");
  const csvTask = await heldTask("csv");
  assert.match(csvTask, new RegExp(pdfTask));
  assert(!/^csv\b/.test(csvTask.trim()), "The thread's name is not the task");
  await capture("30-implement-slash-orchestrator", null);
  await stopLoop("csv");
  console.log(
    "PASS: /implement <thread> <task> in the orchestrator's composer starts the named thread's loop",
  );

  // ---------------------------------------------------------------------------------
  // 10. sweep = "runs": the editor's Sweep choice and its hint; the card's `auditing N of M
  //     rules`; the inspector's rules, their audits and each rule's verdict; the end
  //     notifies once, with how it ended.
  // ---------------------------------------------------------------------------------
  writeFileSync(join(templates, "triage.md"), FIX_TRIAGE);
  writeFileSync(join(templates, "rules.md"), RULES);
  writeFileSync(join(templates, "sweep-verify.md"), SWEEP_VERIFY);
  await page.evaluate(async () => {
    await window.twr.rpc("thread.new", {
      project: "demo",
      name: "sweeper",
      runtime: "fake",
      mode: "auto",
    });
    await window.twr.rpc("thread.send", {
      thread: "sweeper",
      prompt:
        "!write parse.rs pub fn parse(s: &str) -> usize { s.len() }\nWrote it.",
    });
  });
  await until(() => {
    const t = thread("sweeper");
    return t && t.status === "done" && Number(t.git?.dirty_files) > 0;
  }, "sweeper's turn never ended with changes");
  const swCard = () =>
    page.locator(".thread-list .thread-item").filter({ hasText: "sweeper" });
  const selectSweeper = async () => {
    await sidebar();
    await swCard().locator(".thread-card").click();
    await inspector.waitFor();
  };
  await selectSweeper();
  await page.getByRole("button", { name: "Talk to this thread" }).click();
  const swBox = page.getByRole("textbox", { name: "Message sweeper directly" });
  await swBox.fill("/loop settings --project demo");
  await swBox.press("Enter");
  const sweepEditor = page.getByRole("dialog", {
    name: "Loop settings · demo",
  });
  await sweepEditor.waitFor();
  await sweepEditor
    .getByText("Loading loop settings…")
    .waitFor({ state: "hidden" });
  const sweepField = sweepEditor.getByRole("combobox", {
    name: "Sweep",
    exact: true,
  });
  const hintOf = (locator) =>
    locator.evaluate(
      (n) =>
        document.getElementById(n.getAttribute("aria-describedby"))
          ?.textContent || "",
    );
  await sweepField.selectOption("thread");
  assert.equal(
    await hintOf(sweepField),
    "The thread audits each rule itself, with its own subagents where it has them.",
  );
  await sweepField.selectOption("runs");
  assert.equal(
    await hintOf(sweepField),
    "One read-only reviewer call per rule, a few at once; the thread verifies their reports.",
  );
  await sweepEditor
    .getByRole("combobox", { name: "Profile", exact: true })
    .selectOption("medium");
  await sweepEditor
    .getByRole("group", { name: "Save for" })
    .getByRole("button", { name: "demo only" })
    .click();
  await dismissNotices();
  await sweepField.scrollIntoViewIfNeeded();
  await ariaOf("31-editor-sweep", sweepEditor);
  await capture("31-editor-sweep", null, {
    restore: () => sweepField.scrollIntoViewIfNeeded(),
  });
  await sweepEditor.getByRole("button", { name: "Save settings" }).click();
  await sweepEditor.getByText("Saved. Loops start on these settings").waitFor();
  await sweepEditor
    .getByRole("button", { name: "Close", exact: true })
    .last()
    .click();
  const demoSettings = twrJson("loop", "settings", "--project", "demo");
  assert.equal(demoSettings.settings.sweep, "runs");
  assert.equal(demoSettings.settings.profile, "medium");
  console.log(
    "PASS: the editor's Sweep choice says what each does and saves sweep = runs",
  );
  // m1 finds one problem, which the thread fixes as a new class; m2 finds none; the
  // rules turn names two rules; their audits run one at a time (`max_audit_runs = 1`):
  // s1 finds a place that breaks its rule, s2 none; v1 finds nothing.
  const base = runs();
  const [m1, s1, s2, v1] = [base + 1, base + 3, base + 4, base + 5];
  answer(m1, [
    {
      severity: "major",
      location: "parse.rs:1",
      problem: "parse panics on empty input",
      fix: "guard it",
    },
  ]);
  answer(s1, [
    {
      severity: "minor",
      location: "parse.rs:1",
      problem: "parse reads its input unguarded",
      fix: "return early on empty input",
    },
  ]);
  hold(s1);
  hold(s2);
  hold(v1);
  await swBox.fill("/review");
  await swBox.press("Enter");
  await until(() => thread("sweeper").loop, "/review started no loop");
  assert.equal(await setupItem(), undefined, "Saved settings: no setup card");
  await until(() => runs() >= s1, "The first audit never started", 60000);
  await sidebar();
  const swChip = swCard().locator(".loop-chip");
  await swChip
    .locator(".loop-line")
    .filter({ hasText: /^medium · sweep · auditing 0\/2/ })
    .waitFor({ timeout: 15000 });
  await swCard()
    .locator(".thread-card-summary")
    .filter({ hasText: "auditing 1 of 2 rules" })
    .waitFor({ timeout: 15000 });
  await selectSweeper();
  const swLoops = inspector.locator(".inspector-loops");
  const audits = swLoops.getByRole("list", { name: "Sweep audits" });
  await audits.waitFor({ timeout: 15000 });
  await audits
    .getByText(/^s1 guard-input · auditing$/)
    .waitFor({ timeout: 15000 });
  await audits.getByText(/^s2 trim-input · waiting for a slot$/).waitFor();
  assert.equal(
    await swLoops.locator('.loop-phases [aria-current="step"]').innerText(),
    "sweep",
  );
  await ariaOf("32-sweep-auditing-card", swCard());
  await ariaOf("32-sweep-auditing", swLoops);
  await capture("32-sweep-auditing", null, {
    narrow: inspectorNarrow(swLoops),
    restore: selectSweeper,
  });
  console.log(
    "PASS: while the rules are audited the card says auditing 1 of 2 rules, the loop line auditing 0/2, the inspector each rule's audit",
  );
  release(s1);
  await until(() => runs() >= s2, "The second audit never started");
  await audits
    .getByText(/^s1 guard-input · 1 finding$/)
    .waitFor({ timeout: 15000 });
  await audits.getByText(/^s2 trim-input · auditing$/).waitFor();
  await sidebar();
  await swChip
    .locator(".loop-line")
    .filter({ hasText: /auditing 1\/2/ })
    .waitFor({ timeout: 15000 });
  release(s2);
  // The verification turn reports each rule; v1 is held, to look at the verdicts.
  await until(() => runs() >= v1, "The verification run never started", 60000);
  await selectSweeper();
  await audits
    .getByText(/^s1 guard-input · 1 finding · fixed$/)
    .waitFor({ timeout: 15000 });
  await audits
    .getByText(/^s2 trim-input · no place breaks it · checked clean$/)
    .waitFor();
  await audits.locator("summary").first().click();
  await audits
    .getByText("Every parser guards empty input. (where: lib.rs)")
    .waitFor();
  await audits.getByText("parse reads its input unguarded").waitFor();
  await ariaOf("33-sweep-verdicts", swLoops);
  await capture("33-sweep-verdicts", null, {
    narrow: inspectorNarrow(audits),
    restore: selectSweeper,
  });
  console.log(
    "PASS: the inspector lists each rule with its audit's findings and the thread's verdict",
  );
  // The end, with the window hidden: one notification, with how it ended.
  await hidden(async () => {
    await page.waitForTimeout(1500);
    release(v1);
    await until(
      () => !thread("sweeper").loop && thread("sweeper").last_loop,
      "The sweep loop never ended",
      60000,
    );
    assert.equal(thread("sweeper").last_loop.outcome, "clean");
    const notice = await until(async () => {
      await send({ type: "inbox-poll" });
      return (await notifications()).find(
        (n) => n.title === "sweeper · default",
      );
    }, "No notification of sweeper's loop's end");
    assert.match(notice.body, /^Review loop clean after \d+ rounds$/);
    for (let i = 0; i < 3; i++) {
      await send({ type: "inbox-poll" });
      await page.waitForTimeout(700);
    }
    assert.equal(
      (await notifications()).filter((n) => n.title === "sweeper · default")
        .length,
      1,
      "sweeper's end notifies once",
    );
  });
  const swReport = twr("loop", "report", "sweeper");
  assert.match(swReport, /guard-input/);
  console.log(
    "PASS: a review loop audited by runs ends clean and notifies once: Review loop clean after N rounds",
  );

  // ---------------------------------------------------------------------------------
  // 11. Loops on a thread on a remote box: a dial-in `twr runner` of box `lab` on this
  //     machine (its own TWR_HOME), with its own fake Codex and Claude Code, which its
  //     agent probe finds (`TWR_*_BIN`). The setup card's warnings name the box; a
  //     reviewer account other than the box's own login does not hold there; the runner
  //     stopped during a review run makes the loop wait on its box, in words, said once;
  //     started again, the loop goes on from the same round, without a reload.
  // ---------------------------------------------------------------------------------
  const freePort = () =>
    new Promise((done, fail) => {
      const server = createServer();
      server.once("error", fail);
      server.listen(0, "127.0.0.1", () => {
        const { port } = server.address();
        server.close(() => done(port));
      });
    });
  const boxPort = await freePort();
  const boxToken = twr("runner", "token").trim();
  mkdirSync(boxHome, { recursive: true });
  writeFileSync(join(boxHome, "lab.token"), boxToken + "\n", { mode: 0o600 });
  const boxRevs = join(home, "box-reviewers");
  mkdirSync(boxRevs, { recursive: true });
  // The box's fake Codex: its agent probe's `--version` and `login status`, then review runs
  // as the hub's fake answers them (`hold.<n>` holds run n; every run finds nothing).
  const boxCodex = join(boxRevs, "codex");
  writeFileSync(
    boxCodex,
    `#!/bin/sh
here=$(dirname "$0")
case "$1" in --version) echo "codex-cli 0.99.0"; exit 0;; login) exit 0;; esac
n=$(( $(cat "$here/count" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$here/count"
cat > /dev/null
while [ -f "$here/hold.$n" ]; do sleep 0.05; done
out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
printf '%s' '{"findings": []}' > "$out"
echo '{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":10}}'
`,
  );
  chmodSync(boxCodex, 0o755);
  // The box's Claude Code answers only its agent probe: it reviews nothing here.
  const boxClaude = join(boxRevs, "claude");
  writeFileSync(
    boxClaude,
    `#!/bin/sh
case "$1" in --version) echo "2.0.0 (Claude Code)"; exit 0;; auth) echo '{"loggedIn": true, "authMethod": "claude.ai"}'; exit 0;; esac
exit 1
`,
  );
  chmodSync(boxClaude, 0o755);
  const boxRuns = () => {
    try {
      return Number(readFileSync(join(boxRevs, "count"), "utf8").trim());
    } catch {
      return 0;
    }
  };
  const boxHold = (n) => writeFileSync(join(boxRevs, `hold.${n}`), "");
  const boxRelease = (n) => rmSync(join(boxRevs, `hold.${n}`), { force: true });
  // The ACP presets (step 12), on the scripted ACP reviewer of the hub's tests: `ex` reviews
  // in its agent's "read-only" mode; `bare` names no review mode; `loose` names one twr knows
  // lets the agent act.
  const acpState = join(home, "acp");
  mkdirSync(acpState, { recursive: true });
  const acpAgent = `python3 ${resolve("../crates/twr-hub/tests/fixtures/acp/reviewer_agent.py")} ${acpState}`;
  const acpPresets = (withEx) =>
    (withEx
      ? `[runtimes.ex]\nacp = ${JSON.stringify(acpAgent)}\n[runtimes.ex.review]\nmode = "read-only"\nmodels = ["m-large", "m-small"]\nefforts = ["medium", "xhigh"]\n`
      : "") +
    `[runtimes.bare]\nacp = ${JSON.stringify(acpAgent)}\n[runtimes.loose]\nacp = ${JSON.stringify(acpAgent)}\n[runtimes.loose.review]\nmode = "default"\n`;
  // Codex has a second account, `work`, on the hub's machine.
  const workAccount = join(home, "accounts", "codex-work");
  mkdirSync(workAccount, { recursive: true, mode: 0o700 });
  const remoteConfig = (withEx = true) =>
    config(true) +
    `[hub]\nlisten = "127.0.0.1:${boxPort}"\n[boxes.lab]\ntoken = "${boxToken}"\n[accounts.codex.work]\ndir = ${JSON.stringify(workAccount)}\n` +
    acpPresets(withEx);
  const restartHub = async (text) => {
    twr("hub", "stop");
    await new Promise((r) => hub.once("exit", r));
    writeFileSync(join(home, "config.toml"), text, { mode: 0o600 });
    await startHub();
    await connected();
  };
  await restartHub(remoteConfig());
  const settingsOf = (name) =>
    page.evaluate(
      (thread) => window.twr.rpc("loop.settings.get", { thread }),
      name,
    );
  /** The box's runner, with Codex installed there or not (its probe then finds none). */
  const startRunner = async ({ codex = true } = {}) => {
    const runnerEnv = {
      ...env,
      TWR_HOME: boxHome,
      TWR_TEST_USER_TOKEN_FILE: join(boxHome, "user.token"),
      TWR_BOX_PROBE: "on",
      TWR_CODEX_BIN: codex ? boxCodex : join(boxRevs, "no-codex"),
      TWR_CLAUDE_BIN: boxClaude,
      TWR_GROK_BIN: join(boxRevs, "no-grok"),
      XDG_STATE_HOME: join(boxHome, "state"),
    };
    delete runnerEnv.TWR_TEST_AGENTS;
    runner = spawn(
      join(bin, "twr"),
      [
        "runner",
        "--hub",
        `ws://127.0.0.1:${boxPort}`,
        "--box=lab",
        `--token-file=${join(boxHome, "lab.token")}`,
        `--data-dir=${join(boxHome, "data")}`,
      ],
      { env: runnerEnv, stdio: ["ignore", "ignore", "pipe"] },
    );
    runner.stderr.on("data", (d) => (runnerLog += d));
    await until(
      () => {
        try {
          return twrJson("box", "status", "lab").state === "online";
        } catch {
          return false;
        }
      },
      `Box lab never came online: ${runnerLog.slice(-2000)}`,
    );
  };
  /** Waits until the hub has the box's agents as its runner found them. */
  const boxAgents = (codexFound) =>
    until(async () => {
      const got = await settingsOf("boxer");
      const codex = got.runtimes.find((r) => r.runtime === "codex");
      return got.box === "lab" && codex?.probed && codex.found === codexFound;
    }, "Box lab never reported its agents");
  const stopRunner = async () => {
    if (!runner || runner.exitCode !== null) return;
    runner.kill("SIGTERM");
    await new Promise((r) => runner.once("exit", r));
  };
  const boxRepo = join(home, "box-repo");
  mkdirSync(boxRepo);
  /** Commits `paths` (every change, without) in `dir`, as the thread would. */
  const commitIn = (dir, message, paths = ["-A"]) => {
    execFileSync("git", ["-C", dir, "add", ...paths], { stdio: "ignore" });
    execFileSync(
      "git",
      [
        "-C",
        dir,
        "-c",
        "user.name=Desktop Test",
        "-c",
        "user.email=desktop@example.invalid",
        "commit",
        "-q",
        "-m",
        message,
      ],
      { stdio: "ignore" },
    );
  };
  execFileSync("git", ["init", "-q", "-b", "main", boxRepo]);
  writeFileSync(join(boxRepo, "lib.rs"), "pub fn f() {}\n");
  commitIn(boxRepo, "Initial");
  await startRunner({ codex: false });
  twr(
    "thread",
    "new",
    boxRepo,
    "--box",
    "lab",
    "--runtime",
    "fake",
    "--name",
    "boxer",
    "--mode",
    "auto",
  );
  const boxer = () => thread("boxer");
  await until(() => boxer()?.worktree, "boxer has no worktree");
  writeFileSync(join(boxer().worktree, "lib.rs"), "fn parse(s: &str) {}\n");
  await boxAgents(false);
  twr("loop", "settings", "--reset", "--yes");
  const boxerCard = () =>
    page.locator(".thread-list .thread-item").filter({ hasText: "boxer" });
  const selectBoxer = async () => {
    await sidebar();
    await boxerCard().locator(".thread-card").click();
    await inspector.waitFor();
    await inspector.getByRole("heading", { name: "boxer" }).waitFor();
  };
  /** WCAG contrast of an element's text on the first opaque background behind it. */
  const textContrast = (locator) =>
    locator.evaluate(async (node) => {
      await new Promise((done) =>
        requestAnimationFrame(() => requestAnimationFrame(done)),
      );
      const rgba = (value) => value.match(/[\d.]+/g).map(Number);
      let back = node;
      let bg = rgba(getComputedStyle(back).backgroundColor);
      while (back.parentElement && bg.length === 4 && bg[3] === 0) {
        back = back.parentElement;
        bg = rgba(getComputedStyle(back).backgroundColor);
      }
      const lum = (c) =>
        c
          .slice(0, 3)
          .map((v) => v / 255)
          .map((v) => (v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4))
          .reduce((sum, v, i) => sum + v * [0.2126, 0.7152, 0.0722][i], 0);
      const [a, b] = [lum(rgba(getComputedStyle(node).color)), lum(bg)];
      return (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
    });
  /** `locator`'s text reads at 4.5:1 in the light and the dark theme. */
  const readsInBoth = async (locator, where) => {
    for (const value of ["light", "dark"]) {
      await theme(value);
      const ratio = await textContrast(locator);
      assert(
        ratio >= 4.5,
        `${where} reads at ${ratio.toFixed(2)}:1 (${value})`,
      );
    }
    await theme("dark");
  };
  const optionsOf = (select) =>
    select.evaluate((n) =>
      [...n.options].map((o) => ({
        value: o.value,
        text: o.textContent,
        disabled: o.disabled,
      })),
    );
  const armed = async (card) => {
    await card.locator(".attention-options").scrollIntoViewIfNeeded();
    await card.locator('.attention-options[data-armed="true"]').waitFor();
  };

  // The first loop on the box, whose runner found no Codex: the card reviews with Claude
  // Code, and its warning names the box.
  await selectBoxer();
  const boxLoops = inspector.locator(".inspector-loops");
  await boxLoops
    .getByRole("button", { name: "Review loop", exact: true })
    .click();
  const boxSetup = setupIn(inspector);
  await boxSetup.waitFor({ timeout: 15000 });
  await boxSetup
    .getByText(
      "codex isn't installed on box lab: reviewing with claude-code · claude-opus-5-5 instead",
    )
    .waitFor();
  assert.match(
    await boxSetup.locator(".loop-setup-rows").innerText(),
    /Reviewer\s+claude-code · claude-opus-5-5 · medium → xhigh/,
  );
  await ariaOf("40-box-not-installed", boxSetup);
  await capture("40-box-not-installed", null, {
    narrow: inspectorNarrow(boxSetup),
    restore: selectBoxer,
  });
  // Change… : the editor checks what the box has, and says which reviewers can't review
  // there and why, in words.
  await armed(boxSetup);
  await boxSetup.getByRole("button", { name: "Change…" }).click();
  const boxEditor = page.getByRole("dialog", { name: "Loop settings · boxer" });
  await boxEditor.waitFor();
  await boxEditor
    .getByText("Loading loop settings…")
    .waitFor({ state: "hidden" });
  await boxEditor
    .getByText("Runtimes as found on box lab: its reviewer runs there.")
    .waitFor();
  const boxSkipped = boxEditor.locator(".loop-editor-skipped");
  assert.equal(
    await boxSkipped.innerText(),
    `Can't review on box lab: codex (not installed); grok-build (not installed); bare (acp preset bare names no read-only mode); loose (acp preset loose's review mode "default" is not read-only: twr knows it as a mode that lets the agent act)`,
  );
  await readsInBoth(boxSkipped, "The reviewers that can't review");
  await ariaOf("41-box-editor", boxEditor);
  await capture("41-box-editor", null, { narrow: false });
  await page.keyboard.press("Escape");
  await boxEditor.waitFor({ state: "hidden" });
  await armed(boxSetup);
  await boxSetup.getByRole("button", { name: "Cancel" }).click();
  await boxSetup.waitFor({ state: "hidden" });
  console.log(
    "PASS: a box thread's setup card names the box when its reviewer is not installed there; the editor says what can't review there",
  );

  // Saved settings whose reviewer is Codex's account `work`: on the box, which runs every
  // runtime on its own login, they do not hold. The card comes back on the default account
  // and says why.
  await stopRunner();
  await startRunner({ codex: true });
  await boxAgents(true);
  await page.evaluate(() =>
    window.twr.rpc("loop.settings.set", {
      settings: {
        profile: "low",
        implementer: null,
        workers: { model: "claude-opus-5-5", effort: "high" },
        reviewer: { runtime: "codex", account: "work", model: "gpt-6.1-sol" },
      },
    }),
  );
  assert.equal(twrJson("loop", "settings").settings.reviewer.account, "work");
  boxHold(1);
  await selectBoxer();
  await boxLoops
    .getByRole("button", { name: "Review loop", exact: true })
    .click();
  await boxSetup.waitFor({ timeout: 15000 });
  assert.match(
    await boxSetup.locator(".loop-setup-title").innerText(),
    /^boxer: review loop setup \(a saved setting no longer holds\)/,
  );
  await boxSetup
    .getByText(
      "box lab runs codex on its own login, not account work: using its default account",
    )
    .waitFor();
  assert.match(
    await boxSetup.locator(".loop-setup-rows").innerText(),
    /Reviewer\s+codex · gpt-6\.1-sol · medium\n/,
  );
  await ariaOf("42-box-account", boxSetup);
  await capture("42-box-account", null, {
    narrow: inspectorNarrow(boxSetup),
    restore: selectBoxer,
  });
  console.log(
    "PASS: a reviewer account other than the box's own login comes back on the card with the default account, saying why",
  );

  // Start and save: the review run on the box is held, then its runner is stopped.
  await armed(boxSetup);
  await boxSetup.getByRole("button", { name: "Start and save" }).click();
  await until(() => boxRuns() === 1, "The box's review run never started");
  assert.equal(
    twrJson("loop", "settings").settings.reviewer.account ?? null,
    null,
    "Start and save saved the default account",
  );
  await sidebar();
  const boxChip = boxerCard().locator(".loop-chip");
  await boxChip
    .locator(".loop-line")
    .filter({ hasText: /^low · ladder m1$/ })
    .waitFor();
  await selectBoxer();
  const boxStatus = boxLoops.locator(".loop-status");
  await boxStatus.filter({ hasText: "waiting on the review" }).waitFor();
  const boxRounds = boxLoops.getByRole("list", { name: "Rounds" });
  await boxRounds.getByText("m1 under review").waitFor();
  const boxControls = boxLoops.getByRole("group", { name: "Loop controls" });
  const pauseTop = () =>
    boxControls
      .getByRole("button", { name: /^(Pause|Resume)$/ })
      .evaluate(
        (n) =>
          n.getBoundingClientRect().top -
          n.closest(".inspector-loops").getBoundingClientRect().top,
      );
  const runningTop = await pauseTop();
  await stopRunner();
  await until(
    () => boxer().loop?.box_away === "lab",
    "The loop never waited on its box",
  );
  // The loop line on the card and the inspector say so, in words, by the box's name.
  await boxChip
    .locator(".loop-line")
    .filter({ hasText: /^low · ladder m1 · waiting on box lab$/ })
    .waitFor();
  await boxStatus
    .filter({ hasText: /REVIEW LOOP\s+low · waiting on box lab$/ })
    .waitFor();
  await boxRounds.getByText("m1 waits for box lab").waitFor();
  assert.equal(
    await pauseTop(),
    runningTop,
    "The Loops section's controls stay where they were",
  );
  await announced(
    "boxer's review loop waits on box lab: it goes on when the box is back",
  );
  // Its controls work while the box is away: the hub holds them, not the box.
  await boxControls.getByRole("button", { name: "Pause", exact: true }).click();
  await boxStatus.filter({ hasText: "paused, waiting on box lab" }).waitFor();
  await announced("Loop paused");
  assert.equal(twrJson("loop", "list")[0].paused, true);
  await boxControls
    .getByRole("button", { name: "Resume", exact: true })
    .click();
  await boxStatus.filter({ hasText: /waiting on box lab$/ }).waitFor();
  await announced("Loop resumed");
  assert.equal(
    (await live()).split(" | ").filter((s) => s.includes("waits on box lab"))
      .length,
    1,
    "The wait is said once",
  );
  await ariaOf("43-box-away", boxLoops);
  await capture("43-box-away", null, {
    narrow: inspectorNarrow(boxLoops),
    restore: selectBoxer,
  });
  console.log(
    "PASS: the runner stopped during a review run: the loop line and the inspector say the loop waits on box lab, said once; its controls still work",
  );

  // The app opened while the loop waits (the renderer reloaded: its state comes from the
  // hub's snapshot), then its own connection dropped and back (the hub restarted).
  const listen = () =>
    page.evaluate(() => {
      window.__said = [];
      new MutationObserver(() => {
        for (const node of document.querySelectorAll("[data-twr-live]"))
          if (node.textContent && window.__said.at(-1) !== node.textContent)
            window.__said.push(node.textContent);
      }).observe(document.body, {
        subtree: true,
        childList: true,
        characterData: true,
      });
    });
  await page.reload();
  await listen();
  await connected();
  await sidebar();
  await boxChip
    .locator(".loop-line")
    .filter({ hasText: /waiting on box lab$/ })
    .waitFor();
  await selectBoxer();
  await boxStatus.filter({ hasText: /waiting on box lab$/ }).waitFor();
  await boxRounds.getByText("m1 waits for box lab").waitFor();
  twr("hub", "stop");
  await new Promise((r) => hub.once("exit", r));
  await startHub();
  await connected();
  await selectBoxer();
  await boxStatus.filter({ hasText: /waiting on box lab$/ }).waitFor();
  await page.waitForTimeout(1500);
  assert(
    !(await live()).includes("waits on box lab"),
    "A wait the app opened on is shown, not said again",
  );
  console.log(
    "PASS: opened, and reconnected, while the loop waits: the app shows the wait from the snapshot",
  );

  // The runner started again: the loop goes on from the same round, without a reload.
  boxRelease(1);
  boxHold(2);
  await startRunner({ codex: true });
  await until(() => boxRuns() === 2, "The round's retry never started");
  await announced("boxer's review loop goes on: box lab is back");
  await boxChip
    .locator(".loop-line")
    .filter({ hasText: /^low · ladder m1$/ })
    .waitFor();
  await boxStatus
    .filter({ hasText: /REVIEW LOOP\s+low · waiting on the review$/ })
    .waitFor();
  await boxRounds.getByText("m1 under review again").waitFor();
  assert.equal(await boxRounds.locator("li").count(), 1, "Still round m1");
  await capture("44-box-back", boxLoops, {
    narrow: inspectorNarrow(boxLoops),
    restore: selectBoxer,
  });
  boxRelease(2);
  await until(
    () => !boxer().loop && boxer().last_loop,
    "The box's review loop never ended",
    60000,
  );
  assert.equal(boxer().last_loop.outcome, "clean");
  await announced("boxer's review loop ended: clean");
  await boxLoops.getByText("Last review loop: clean").waitFor();
  await boxLoops.getByRole("button", { name: "Open report" }).click();
  const boxReport = page.getByRole("dialog", {
    name: "Review loop report · boxer",
  });
  await boxReport.waitFor();
  await boxReport
    .getByText("m1: 0 findings; failed run: box lab disconnected")
    .waitFor();
  await ariaOf("45-box-report", boxReport);
  await capture("45-box-report", null, { narrow: false });
  await page.keyboard.press("Escape");
  await boxReport.waitFor({ state: "hidden" });
  console.log(
    "PASS: the runner back, the loop goes on from round m1 without a reload, ends clean and its report says the run that failed",
  );

  // An implementation loop on the box thread, sent from its composer, on the saved settings.
  // Its commit names the box thread's repository.
  writeFileSync(
    join(templates, "impl-report.md"),
    '!sleep 4000\nBuilt the CSV export.\n{{commit}}\n```twr-commit\n{"commits": [{"repo": "box-repo", "sha": "abc1234"}]}\n```\n',
  );
  await selectBoxer();
  await page.getByRole("button", { name: "Talk to this thread" }).click();
  const boxerBox = page.getByRole("textbox", {
    name: "Message boxer directly",
  });
  await boxerBox.waitFor();
  await page.getByText("Loading conversation…").waitFor({ state: "hidden" });
  await boxerBox.fill("add a CSV export to the reports page");
  await page.getByRole("button", { name: "More ways to send" }).click();
  await page
    .getByRole("button", { name: "Send as implementation loop", exact: true })
    .click();
  await until(
    () => boxer().loop?.kind === "implement",
    "The box's implementation loop never started",
  );
  await sidebar();
  await boxChip
    .locator(".loop-badge")
    .filter({ hasText: "IMPLEMENT LOOP" })
    .waitFor();
  await until(
    () => boxer().loop?.phase === "report",
    "The box's implementation loop never reached its report",
    90000,
  );
  // Only the loop's work: the review's change before it is the baseline, not its to commit.
  commitIn(boxer().worktree, "Add the CSV export", ["feature.rs"]);
  await until(
    () => !boxer().loop && boxer().last_loop?.kind === "implement",
    "The box's implementation loop never ended",
    60000,
  );
  const boxLast = boxer().last_loop;
  assert.equal(boxLast.outcome, "clean");
  assert.equal(boxLast.commits?.length, 1, JSON.stringify(boxLast));
  const boxSha = boxLast.commits[0].sha.slice(0, 7);
  await announced(
    `boxer's implementation loop ended: clean; committed ${boxSha}`,
  );
  await boxerCard()
    .locator(".loop-ended")
    .filter({ hasText: `committed ${boxSha}` })
    .waitFor();
  await selectBoxer();
  await boxLoops.getByText("Last implementation loop: clean").waitFor();
  writeFileSync(
    join(templates, "impl-report.md"),
    '!sleep 4000\nBuilt the CSV export.\n{{commit}}\n```twr-commit\n{"commits": [{"repo": "demo", "sha": "abc1234"}]}\n```\n',
  );
  console.log(
    "PASS: an implementation loop on the box thread, sent from its composer, plans, implements, reviews on the box and commits",
  );

  // The box away with no loop running: nothing offers to start one (the hub refuses it).
  await stopRunner();
  await until(() => boxer().box_online === false, "Box lab never went offline");
  await selectBoxer();
  const startWhy = boxLoops.locator(".loop-start-why");
  await startWhy
    .filter({ hasText: "Box lab is away: a loop can start once it is back." })
    .waitFor();
  for (const name of ["Review loop", "Implementation loop…"])
    assert(
      await boxLoops.getByRole("button", { name, exact: true }).isDisabled(),
      `${name} is disabled while the box is away`,
    );
  await readsInBoth(startWhy, "Why no loop can start");
  await ariaOf("46-box-offline-start", boxLoops);
  await capture("46-box-offline-start", boxLoops, { narrow: false });
  await page.getByRole("button", { name: "Talk to this thread" }).click();
  await boxerBox.fill("another task");
  await page.getByRole("button", { name: "More ways to send" }).click();
  const awayChoice = page.getByRole("button", {
    name: "Send as implementation loop",
    exact: true,
  });
  assert(await awayChoice.isDisabled(), "Send as implementation loop waits");
  await page
    .locator(".send-loop-panel small")
    .filter({ hasText: "Box lab is away: a loop can start once it is back." })
    .waitFor();
  // The choice can't be made: the focus goes to its panel, which says why, and Escape
  // closes it, giving the focus back to the caret (not leaving the conversation).
  assert(
    await page
      .locator(".send-loop-panel")
      .evaluate((n) => n === document.activeElement),
    "The panel that says why has the focus",
  );
  await page.keyboard.press("Escape");
  await page.locator(".send-loop-panel").waitFor({ state: "hidden" });
  assert.equal((await focused()).text, "More ways to send");
  await boxerBox.fill("");
  console.log(
    "PASS: with the box away, Review loop, Implementation loop… and Send as implementation loop say why they wait",
  );

  // ---------------------------------------------------------------------------------
  // 12. An ACP preset as the reviewer: the pickers list every preset, the one that can't
  //     review disabled and named with why; the card says what its read-only rests on; a
  //     loop on it runs to its end; the preset removed from the config, the card says so.
  // ---------------------------------------------------------------------------------
  const acpHold = (n) => writeFileSync(join(acpState, `hold.${n}`), "");
  const acpRelease = (n) =>
    rmSync(join(acpState, `hold.${n}`), { force: true });
  await page.evaluate(async () => {
    await window.twr.rpc("loop.settings.reset", {});
    await window.twr.rpc("thread.new", {
      project: "demo",
      name: "acpw",
      runtime: "fake",
      mode: "auto",
    });
    await window.twr.rpc("thread.send", {
      thread: "acpw",
      prompt: "!write lib.rs pub fn g() { todo!() }\nWrote it.",
    });
  });
  await until(
    () =>
      thread("acpw")?.status === "done" &&
      Number(thread("acpw").git?.dirty_files) > 0,
    "acpw's turn never ended with changes",
  );
  const acpCard = () =>
    page.locator(".thread-list .thread-item").filter({ hasText: "acpw" });
  const selectAcp = async () => {
    await sidebar();
    await acpCard().locator(".thread-card").click();
    await inspector.waitFor();
    await inspector.getByRole("heading", { name: "acpw" }).waitFor();
  };
  const acpLoops = inspector.locator(".inspector-loops");
  /** The editor's reviewer and implementer pickers in `where`, as a person finds them. */
  const pickers = async (where, label) => {
    const reviewer = where
      .getByRole("group", { name: "Reviewer" })
      .getByRole("combobox", { name: "Runtime" });
    const implementer = where.getByRole("combobox", {
      name: "Implementer",
      exact: true,
    });
    const options = await optionsOf(reviewer);
    const named = (value) => options.find((o) => o.value === value);
    assert.deepEqual(named("ex"), {
      value: "ex",
      text: "ex (acp, read-only mode)",
      disabled: false,
    });
    assert.deepEqual(named("bare"), {
      value: "bare",
      text: "bare (acp preset bare names no read-only mode)",
      disabled: true,
    });
    assert.deepEqual(named("loose"), {
      value: "loose",
      text: `loose (acp preset loose's review mode "default" is not read-only: twr knows it as a mode that lets the agent act)`,
      disabled: true,
    });
    assert.deepEqual(
      (await optionsOf(implementer))
        .map((o) => o.value)
        .filter((v) => ["ex", "bare", "loose"].includes(v)),
      [],
      `${label}: no ACP preset is offered as the implementer`,
    );
    const skipped = where.locator(".loop-editor-skipped");
    assert.equal(
      await skipped.innerText(),
      `Can't review: grok-build (not installed); bare (acp preset bare names no read-only mode); loose (acp preset loose's review mode "default" is not read-only: twr knows it as a mode that lets the agent act)`,
    );
    await readsInBoth(skipped, `${label}: the reviewers that can't review`);
    // A screen reader hears the reasons with the picker.
    const described = await reviewer.evaluate((n) =>
      (n.getAttribute("aria-describedby") || "")
        .split(" ")
        .map((id) => document.getElementById(id)?.textContent || "")
        .join(" "),
    );
    assert.match(described, /Can't review: grok-build/);
    return reviewer;
  };
  // The setup card's editor.
  await selectAcp();
  await acpLoops
    .getByRole("button", { name: "Review loop", exact: true })
    .click();
  const acpSetup = setupIn(inspector);
  await acpSetup.waitFor({ timeout: 15000 });
  await armed(acpSetup);
  await acpSetup.getByRole("button", { name: "Change…" }).click();
  const acpEditor = page.getByRole("dialog", { name: "Loop settings · acpw" });
  await acpEditor.waitFor();
  await acpEditor
    .getByText("Loading loop settings…")
    .waitFor({ state: "hidden" });
  const cardReviewer = await pickers(acpEditor, "The card's editor");
  await cardReviewer.focus();
  await cardReviewer.selectOption("ex");
  const acpModel = acpEditor
    .getByRole("group", { name: "Reviewer" })
    .getByRole("combobox", { name: "Model" });
  assert.equal(await acpModel.inputValue(), "m-large");
  assert.deepEqual(
    (await optionsOf(acpModel))
      .map((o) => o.value)
      .filter((v) => v.startsWith("m-")),
    ["m-large", "m-small"],
  );
  const acpNote = acpEditor.locator(".loop-editor-acp");
  assert.equal(
    await acpNote.innerText(),
    'ex is an ACP agent: it reviews in its own "read-only" mode, at the efforts medium, xhigh, which every round of the profile must use.',
  );
  await readsInBoth(acpNote, "The ACP reviewer's note");
  await ariaOf("50-acp-editor", acpEditor);
  await capture("50-acp-editor", null, { narrow: false });
  await acpEditor.getByRole("button", { name: "Save to the card" }).click();
  await acpEditor.waitFor({ state: "hidden" });
  await acpSetup
    .getByText(
      `acp reviewer: read-only rests on the agent's own "read-only" mode`,
    )
    .waitFor();
  assert.match(
    await acpSetup.locator(".loop-setup-rows").innerText(),
    /Reviewer\s+ex · m-large · medium → xhigh/,
  );
  await ariaOf("51-acp-card", acpSetup);
  await capture("51-acp-card", null, {
    narrow: inspectorNarrow(acpSetup),
    restore: selectAcp,
  });
  console.log(
    "PASS: the card's editor lists the ACP presets (the one that reviews with its mode, models and efforts; the others disabled, with why, in words); none as implementer; the card warns what read-only rests on",
  );
  // Start and save: the loop runs on the ACP reviewer to its end.
  acpHold(1);
  await armed(acpSetup);
  await acpSetup.getByRole("button", { name: "Start and save" }).click();
  await until(
    () => thread("acpw").loop?.waiting_on === "review",
    "The ACP review run never started",
    60000,
  );
  await sidebar();
  await acpCard()
    .locator(".thread-card")
    .filter({ hasText: "reviewer ex m-large medium" })
    .waitFor({ timeout: 15000 });
  await selectAcp();
  assert.match(
    await acpLoops.locator(".loop-facts").first().innerText(),
    /Reviewer\s+ex · m-large · medium/,
  );
  await capture("52-acp-running", acpLoops, {
    narrow: inspectorNarrow(acpLoops),
    restore: selectAcp,
  });
  acpRelease(1);
  await until(
    () => !thread("acpw").loop && thread("acpw").last_loop,
    "The ACP review loop never ended",
    90000,
  );
  assert.equal(thread("acpw").last_loop.outcome, "clean");
  await acpLoops.getByText("Last review loop: clean").waitFor();
  await acpLoops.getByRole("button", { name: "Open report" }).click();
  const acpReport = page.getByRole("dialog", {
    name: "Review loop report · acpw",
  });
  await acpReport.waitFor();
  await acpReport.getByText("Reviewer: ex m-large").waitFor();
  await page.keyboard.press("Escape");
  await acpReport.waitFor({ state: "hidden" });
  console.log(
    "PASS: a review loop on the ACP reviewer runs to its end, the reviewer named on the card, in the inspector and in the report",
  );
  // Settings › Loops and one project's editor list the same.
  await sidebar();
  await page
    .locator(".sidebar")
    .getByRole("button", { name: "Settings", exact: true })
    .first()
    .click();
  const loopsPanel = page
    .locator(".settings-section")
    .filter({ has: page.getByRole("heading", { name: "Loops", exact: true }) });
  await loopsPanel.waitFor();
  await loopsPanel
    .getByRole("group", { name: "Reviewer" })
    .waitFor({ timeout: 15000 });
  const panelReviewer = await pickers(loopsPanel, "Settings › Loops");
  assert.equal(await panelReviewer.inputValue(), "ex", "The saved reviewer");
  await loopsPanel.locator(".loop-editor-acp").waitFor();
  await loopsPanel.scrollIntoViewIfNeeded();
  await capture("53-acp-settings", loopsPanel, { narrow: false });
  await selectAcp();
  await page.getByRole("button", { name: "Talk to this thread" }).click();
  const acpBox = page.getByRole("textbox", { name: "Message acpw directly" });
  await acpBox.waitFor();
  await acpBox.fill("/loop settings --project demo");
  await acpBox.press("Enter");
  const demoEditor = page.getByRole("dialog", { name: "Loop settings · demo" });
  await demoEditor.waitFor();
  await demoEditor
    .getByText("Loading loop settings…")
    .waitFor({ state: "hidden" });
  await pickers(demoEditor, "A project's editor");
  await capture("54-acp-project", demoEditor, { narrow: false });
  await page.keyboard.press("Escape");
  await demoEditor.waitFor({ state: "hidden" });
  console.log(
    "PASS: Settings › Loops and a project's editor list the ACP presets the same way",
  );
  // The preset removed from the config while it is the saved reviewer: the next loop's
  // card says it is no preset any more and reviews with the replacement.
  await restartHub(remoteConfig(false));
  await selectAcp();
  await acpLoops
    .getByRole("button", { name: "Review loop", exact: true })
    .click();
  await acpSetup.waitFor({ timeout: 15000 });
  await acpSetup
    .getByText(
      "ex is no reviewer: neither a built-in one (claude-code, codex, grok-build) nor an acp preset in the config: reviewing with claude-code · claude-opus-5-5 instead",
    )
    .waitFor();
  assert.match(
    await acpSetup.locator(".loop-setup-rows").innerText(),
    /Reviewer\s+claude-code · claude-opus-5-5 · medium → xhigh/,
  );
  await ariaOf("55-acp-removed", acpSetup);
  await capture("55-acp-removed", null, {
    narrow: inspectorNarrow(acpSetup),
    restore: selectAcp,
  });
  await armed(acpSetup);
  await acpSetup.getByRole("button", { name: "Cancel" }).click();
  await acpSetup.waitFor({ state: "hidden" });
  console.log(
    "PASS: the saved ACP reviewer removed from the config: the next card says so and offers the replacement",
  );

  if (errors.length) throw new Error(errors.join("\n"));
  console.log(
    "PASS: review and implementation loops in the desktop app against a real hub",
  );
} catch (error) {
  console.error("Failed:", error);
  if (application) {
    const page = await application.firstWindow();
    console.error("Renderer errors:", errors);
    mkdirSync("test-results", { recursive: true });
    await page
      .screenshot({ path: "test-results/loops-failure.png" })
      .catch(() => {});
  }
  console.error("Hub log:", hubLog.slice(-4000));
  if (runnerLog) console.error("Box runner log:", runnerLog.slice(-3000));
  process.exitCode = 1;
} finally {
  if (application)
    await application.evaluate(({ app }) => app.exit(0)).catch(() => {});
  if (applicationProcess?.exitCode === null) applicationProcess.kill("SIGKILL");
  try {
    execFileSync(join(bin, "twr"), ["hub", "stop"], {
      env,
      stdio: "ignore",
      timeout: 15000,
    });
  } catch {
    hub?.kill("SIGTERM");
  }
  if (hub && hub.exitCode === null && hub.signalCode === null)
    await Promise.race([
      new Promise((resolve) => hub.once("exit", resolve)),
      new Promise((resolve) => setTimeout(resolve, 5000)),
    ]);
  if (runner && runner.exitCode === null) runner.kill("SIGKILL");
  stopFixture(join(bin, "twr"), env, { workspaces: [] });
  stopFixture(
    join(bin, "twr"),
    { ...env, TWR_HOME: boxHome },
    { workspaces: [] },
  );
  rmSync(home, {
    recursive: true,
    force: true,
    maxRetries: 20,
    retryDelay: 100,
  });
}
