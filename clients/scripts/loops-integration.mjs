// Review loops in the real Electron app against a real Rust hub (docs/desktop.md › Loops,
// docs/design/loops.md › Desktop and mobile): a fake thread in a git repository, and a fake
// Codex reviewer (`TWR_CODEX_BIN`) that holds a run while `reviewers/hold.<n>` exists and
// fails it while `reviewers/fail.<n>` does. The hub is told Codex is installed and logged in
// (`TWR_TEST_AGENTS`, honoured by a test's throwaway hub only). The fake runtime echoes each
// prompt, so the loop's own prompts answer themselves: the templates in
// `<TWR_HOME>/loops/templates/` end with the block each step asks for.
//
// `TWR_LOOPS_SHOTS=<dir>` also saves, at every step, the window in the light and the dark
// theme at the default size and at the minimum width (`NN-step-<light|dark>.png`,
// `NN-step-min-<light|dark>.png`) and the accessibility tree of the step's region
// (`NN-step.aria.txt`), for a person to look at.
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

const home = realpathSync(mkdtempSync(join(tmpdir(), "twr-loops-")));
const repo = join(home, "repo");
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
const config = (offer) =>
  `default_runtime = "fake"\n[projects.demo]\nrepo = ${JSON.stringify(repo)}\n[orchestrator]\nautostart = false\n[threads]\nidle_compaction = "off"\n[tui]\nsuggest = false\nsummaries = false\n${offer === false ? "[loops]\noffer_review = false\n" : ""}`;
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
if [ "$n" = 1 ]; then
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
          path: join(shots, `${name}-min-${value}.png`),
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
  const compact = (name) =>
    page
      .locator(".compact-nav")
      .getByRole("button", { name, exact: false })
      .first()
      .click();
  /** WCAG contrast of an element's text on its own (opaque) background. */
  const contrast = (locator) =>
    locator.evaluate((node) => {
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
  for (const row of ["Profile", "Scope", "Implementer", "Workers", "Reviewer"])
    assert(rows.includes(row), `The setup card has a ${row} row: ${rows}`);
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
  await capture("06-setup-inline", inlineCard, { narrow: false });
  console.log(
    "PASS: the setup card shows in the inspector, in Needs you and in the conversation, with its five rows and three buttons",
  );

  // ---------------------------------------------------------------------------------
  // 3. Change… opens the editor; a change goes back to the card as a new revision.
  // ---------------------------------------------------------------------------------
  // With the keyboard: Tab from Start and save reaches Change…, Enter opens the editor.
  await inlineCard.locator('.attention-options[data-armed="true"]').waitFor();
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
    narrow: () => compact("More"),
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
    narrow: () => compact("More"),
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
  assert.equal(await openReport.getAttribute("aria-expanded"), "true");
  const path = await loopsSection
    .locator(".loop-report .loop-path code")
    .first()
    .innerText();
  assert.match(path, /report\.md$/);
  assert(existsSync(path), "The report's path exists");
  await page.waitForTimeout(1500);
  assert.equal(
    await page.locator(".loop-offer").count(),
    0,
    "No Review loop offer for changes a loop reviewed",
  );
  await ariaOf("14-ended", inspector);
  await capture("14-ended", null, {
    narrow: () => compact("More"),
    restore: back,
  });
  console.log(
    "PASS: the loop ends clean: the thread's report says so, the chip goes, Open report gives report.md, no offer comes back",
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
  await hidden(async () => {
    await page.waitForTimeout(1500);
    twr("loop", "review", "api", "--setup", "--json");
  }).catch(() => {});
  await notified("Asks you to choose a loop's settings");
  await talk();
  const again = setupIn(page.locator(".conversation-attention"));
  await again.waitFor();
  assert.match(
    await again.locator(".loop-setup-title").innerText(),
    /^api: review loop setup/,
  );
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
  if (errors.length) throw new Error(errors.join("\n"));
  console.log("PASS: review loops in the desktop app against a real hub");
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
  stopFixture(join(bin, "twr"), env, { workspaces: [] });
  rmSync(home, {
    recursive: true,
    force: true,
    maxRetries: 20,
    retryDelay: 100,
  });
}
