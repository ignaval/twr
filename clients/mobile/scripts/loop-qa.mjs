/* Review loops on a phone (docs/design/loops.md › Desktop and mobile), against a real hub
 * with a fake thread and a fake reviewer, as crates/twr-cli/tests/loops.rs drives them.
 *
 * `loopFixture` prepares a QA hub's TWR_HOME: a fake `codex` reviewer that answers one
 * finding on its first run and none after, and holds while `hold.<n>` exists, and the loop
 * templates the fake thread echoes back (each ends with the block its step asks for).
 *
 * `loopQA` and `loopReadOnlyQA` drive the app through a platform driver: android-qa.mjs
 * gives Playwright's WebView page and adb, ios-qa.mjs an XCTest command channel and simctl.
 * Every step saves full-screen PNGs in light and dark appearance and audits the loop
 * surfaces on screen: accessible names, 44-point targets, contrast, clipping and safe areas.
 */
import assert from "node:assert/strict";
import {
  chmodSync,
  mkdirSync,
  rmSync,
  writeFileSync,
  existsSync,
} from "node:fs";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const FAKE_CODEX = `#!/bin/sh
here=$(dirname "$0")
n=$(( $(cat "$here/count" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$here/count"
cat > /dev/null
while [ -f "$here/hold.$n" ]; do sleep 0.05; done
out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
if [ "$n" = 1 ]; then
  printf '%s' '{"findings": [{"severity": "major", "location": "lib.rs:1", "problem": "it breaks on empty input", "fix": "guard it"}]}' > "$out"
else
  printf '%s' '{"findings": []}' > "$out"
fi
echo '{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":10}}'
`;
const TEMPLATES = {
  goal: "Gates are green.\n```twr-goal\nHarden the change against bad input.\n```\n",
  triage:
    '{{findings}}\nTriaged.\n```twr-triage\n{"items": [{"id": "{{finding_ids}}", "verdict": "dismissed", "reason": "empty input never reaches it"}], "gates": {"ran": ["true"], "result": "pass"}}\n```\n',
  report: "The loop reviewed the change; nothing is left open.\n",
};
/** What the hub finds of its agent CLIs: Codex and Claude Code installed and logged in. */
const AGENTS = JSON.stringify([
  { runtime: "codex", found: true, login: "logged_in" },
  { runtime: "claude-code", found: true, login: "logged_in" },
  { runtime: "grok-build", found: false, login: "unknown" },
]);

/** Writes the fake reviewer and templates into `home` (a QA hub's TWR_HOME) and returns
 * the hub environment they need and the reviewer's controls. */
export function loopFixture(home) {
  const reviewers = join(home, "reviewers");
  mkdirSync(reviewers, { recursive: true });
  const codex = join(reviewers, "codex");
  writeFileSync(codex, FAKE_CODEX);
  chmodSync(codex, 0o755);
  const templates = join(home, "loops", "templates");
  mkdirSync(templates, { recursive: true });
  for (const [name, text] of Object.entries(TEMPLATES))
    writeFileSync(join(templates, `${name}.md`), text);
  return {
    env: {
      TWR_CODEX_BIN: codex,
      TWR_TEST_AGENTS: AGENTS,
      TWR_AGENT_PROBE: "off",
    },
    /** Review run `n` (counted from 1 over the hub's life) waits until released. */
    hold: (n) => writeFileSync(join(reviewers, `hold.${n}`), ""),
    release: (n) => rmSync(join(reviewers, `hold.${n}`), { force: true }),
    releaseAll() {
      for (let n = 1; n < 100; n++)
        rmSync(join(reviewers, `hold.${n}`), { force: true });
    },
    /** Review runs started so far. */
    runs: () =>
      existsSync(join(reviewers, "count"))
        ? Number(readFileSync(join(reviewers, "count"), "utf8").trim()) || 0
        : 0,
  };
}

/**
 * The iOS side's command channel: XCTest's `testReviewLoops` long-polls `/loop/next` for a
 * command (JavaScript to run in the app's WKWebView, an orientation) and posts the outcome
 * to `/loop/result`. `handle` serves those routes from ios-qa.mjs's fixture server.
 */
export function commandChannel() {
  let nextId = 1;
  const queue = [];
  const waiting = [];
  const pending = new Map();
  let enabled = false;
  let begun;
  const started = new Promise((resolve) => (begun = resolve));
  const push = (command) => {
    const poll = waiting.shift();
    if (poll) poll(command);
    else queue.push(command);
  };
  return {
    enable() {
      enabled = true;
    },
    get enabled() {
      return enabled;
    },
    /** Resolves when XCTest's testReviewLoops asks whether to run. */
    started,
    call(op, payload = {}, timeoutMs = 120000) {
      const id = nextId++;
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          pending.delete(id);
          reject(new Error(`XCTest did not answer ${op} in ${timeoutMs} ms`));
        }, timeoutMs);
        pending.set(id, (reply) => {
          clearTimeout(timer);
          pending.delete(id);
          const answer = JSON.parse(reply.json || "{}");
          if (answer.error) reject(new Error(answer.error));
          else resolve(answer.value);
        });
        push({ id, op, ...payload });
      });
    },
    finish(message) {
      push(message ? { id: 0, op: "fail", message } : { id: 0, op: "done" });
    },
    /** Serves `/loop/*`; false for any other request. */
    async handle(req, res) {
      if (!req.url?.startsWith("/loop/")) return false;
      res.setHeader("content-type", "application/json");
      if (req.url === "/loop/begin") {
        if (enabled) begun();
        res.statusCode = enabled ? 200 : 404;
        res.end("{}");
      } else if (req.url === "/loop/next") {
        const command =
          queue.shift() ||
          (await new Promise((resolve) => {
            waiting.push(resolve);
            req.on("close", () => {
              const index = waiting.indexOf(resolve);
              if (index >= 0) waiting.splice(index, 1);
            });
          }));
        if (res.destroyed) {
          queue.unshift(command);
          return true;
        }
        res.end(JSON.stringify(command));
      } else if (req.url === "/loop/result" && req.method === "POST") {
        let body = "";
        for await (const chunk of req) body += chunk;
        const reply = JSON.parse(body);
        pending.get(reply.id)?.(reply);
        res.end("{}");
      } else {
        res.statusCode = 404;
        res.end("{}");
      }
      return true;
    },
  };
}

/* Page-side helpers, defined fresh in every evaluation (so a reload loses nothing). */
const HELPERS = String.raw`(() => {
  const visible = (e) => !!e && e.isConnected && e.getClientRects().length > 0 && getComputedStyle(e).visibility !== "hidden" && !e.closest("details:not([open]) > :not(summary)");
  const text = (e) => (e?.innerText ?? e?.textContent ?? "").replace(/\s+/g, " ").trim();
  const nameOf = (e) => {
    const label = e.getAttribute("aria-label");
    if (label?.trim()) return label.trim();
    const by = e.getAttribute("aria-labelledby");
    if (by) {
      const joined = by.split(/\s+/).map((id) => text(document.getElementById(id))).join(" ").trim();
      if (joined) return joined;
    }
    if (e.labels?.length) return [...e.labels].map(text).join(" ").trim();
    if (e.tagName === "FIELDSET") return text(e.querySelector("legend"));
    const own = text(e);
    if (own) return own;
    return (e.getAttribute("title") || "").trim();
  };
  // What sits behind the top modal is inert: no tap or check reaches it.
  const all = (selector, root = document) => [...(root || document).querySelectorAll(selector)].filter((e) => visible(e) && !e.closest("[inert]"));
  const matches = (e, name) => {
    if (name === undefined || name === null) return true;
    const n = nameOf(e);
    if (typeof name === "string") return n === name;
    return new RegExp(name.re, name.flags || "").test(n);
  };
  const find = (selector, name, root) => all(selector, typeof root === "string" ? document.querySelector(root) : root).find((e) => matches(e, name));
  const need = (selector, name, root) => {
    const e = find(selector, name, root);
    if (!e) throw new Error("Not on screen: " + selector + " " + JSON.stringify(name) + "\n" + text(document.body).slice(0, 1500));
    return e;
  };
  const point = (e) => {
    e.scrollIntoView({ block: "center", inline: "nearest" });
    const r = e.getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
  };
  const parse = (value) => {
    const m = value.match(/rgba?\(([^)]+)\)/);
    if (m) {
      const p = m[1].split(/[\s,/]+/).filter(Boolean).map(Number);
      return [p[0], p[1], p[2], p[3] ?? 1];
    }
    const c = value.match(/color\(srgb ([^)]+)\)/);
    if (c) {
      const p = c[1].split(/[\s/]+/).filter(Boolean).map(Number);
      return [p[0] * 255, p[1] * 255, p[2] * 255, p[3] ?? 1];
    }
    return undefined;
  };
  const over = (top, bottom) => {
    const a = top[3] + bottom[3] * (1 - top[3]);
    if (!a) return [0, 0, 0, 0];
    return [0, 1, 2].map((i) => (top[i] * top[3] + bottom[i] * bottom[3] * (1 - top[3])) / a).concat(a);
  };
  const backdrop = (e) => {
    const layers = [];
    for (let n = e; n && n.nodeType === 1; n = n.parentElement) {
      const bg = parse(getComputedStyle(n).backgroundColor);
      if (bg && bg[3] > 0) layers.push(bg);
      if (bg && bg[3] >= 1) break;
    }
    let color = parse(getComputedStyle(document.body).backgroundColor) || [255, 255, 255, 1];
    if (color[3] < 1) color = over(color, [255, 255, 255, 1]);
    for (const layer of layers.reverse()) color = over(layer, color);
    return color;
  };
  const luminance = (c) => {
    const [r, g, b] = c.slice(0, 3).map((v) => {
      const s = v / 255;
      return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
    });
    return 0.2126 * r + 0.7152 * g + 0.0722 * b;
  };
  const ratio = (e) => {
    const bg = backdrop(e);
    let fg = parse(getComputedStyle(e).color);
    if (!fg) return undefined;
    if (fg[3] < 1) fg = over(fg, bg);
    const [a, b] = [luminance(fg), luminance(bg)].sort((x, y) => y - x);
    return (a + 0.05) / (b + 0.05);
  };
  const describe = (e) => (e.tagName.toLowerCase() + (e.className && typeof e.className === "string" ? "." + e.className.trim().split(/\s+/).join(".") : "") + " " + JSON.stringify(nameOf(e).slice(0, 60)));
  const insets = () => {
    const probe = document.createElement("div");
    probe.style.cssText = "position:fixed;visibility:hidden;padding:env(safe-area-inset-top) env(safe-area-inset-right) env(safe-area-inset-bottom) env(safe-area-inset-left)";
    document.body.append(probe);
    const s = getComputedStyle(probe);
    const out = { top: parseFloat(s.paddingTop) || 0, right: parseFloat(s.paddingRight) || 0, bottom: parseFloat(s.paddingBottom) || 0, left: parseFloat(s.paddingLeft) || 0 };
    probe.remove();
    return out;
  };
  const CONTROLS = "button, a[href], input:not([type=hidden]), select, textarea, summary, [role=button], [role=switch], [role=tab]";
  /** What is wrong with the loop surface under root, as seen now (the viewport only). */
  const audit = (rootSelector, options = {}) => {
    const roots = rootSelector ? all(rootSelector) : [document.body];
    const problems = [];
    const safe = insets();
    const vw = document.documentElement.clientWidth, vh = visualViewport?.height || innerHeight;
    for (const root of roots) {
      for (const e of all(CONTROLS, root)) {
        const r = e.getBoundingClientRect();
        if (r.bottom <= 0 || r.top >= vh) continue;
        const name = nameOf(e);
        if (!name) problems.push({ kind: "unnamed control", element: describe(e) });
        if (e.tagName === "DIV" || e.tagName === "SPAN") {
          if (!e.getAttribute("role")) problems.push({ kind: "no role", element: describe(e) });
        }
        // A checkbox's target is its label row; an inline link inside text is exempt.
        const target = e.matches("input[type=checkbox]") ? e.closest("label") || e : e;
        const t = target.getBoundingClientRect();
        if ((t.width < 43.5 || t.height < 43.5) && !options.smallOk?.includes(name))
          problems.push({ kind: "target under 44 pt", element: describe(e), width: Math.round(t.width), height: Math.round(t.height), style: (({ height, minHeight, fontSize, paddingTop, appearance, boxSizing }) => ({ height, minHeight, fontSize, paddingTop, appearance, boxSizing }))(getComputedStyle(target)) });
        if (r.right > vw + 0.5 || r.left < -0.5) problems.push({ kind: "off screen sideways", element: describe(e), left: r.left, right: r.right });
        // Inside the viewport and inside every scrolling ancestor: one scrolled partly out
        // of its pane is not covered, it is scrolled.
        let inside = r.top >= 0 && r.bottom <= vh && r.left >= 0 && r.right <= vw;
        for (let n = e.parentElement; n && inside; n = n.parentElement) {
          if (!/auto|scroll|hidden/.test(getComputedStyle(n).overflowY)) continue;
          const c = n.getBoundingClientRect();
          if (r.top < c.top - 0.5 || r.bottom > c.bottom + 0.5) inside = false;
        }
        if (inside && !e.disabled) {
          const x = Math.min(Math.max(r.x + r.width / 2, 1), vw - 1), y = Math.min(Math.max(r.y + r.height / 2, 1), vh - 1);
          const hit = document.elementFromPoint(x, y);
          if (hit && !e.contains(hit) && !hit.contains(e) && !(e.labels && [...e.labels].some((l) => l.contains(hit))))
            problems.push({ kind: "covered", element: describe(e), by: describe(hit) });
          if (r.top < safe.top - 0.5 || r.bottom > vh - safe.bottom + 0.5 || r.left < safe.left - 0.5 || r.right > vw - safe.right + 0.5)
            problems.push({ kind: "under a system area", element: describe(e), box: r.toJSON(), safe });
        }
      }
      for (const e of all("*", root)) {
        if (e.children.length || !text(e)) continue;
        const r = e.getBoundingClientRect();
        if (r.bottom <= 0 || r.top >= vh) continue;
        const s = getComputedStyle(e);
        if (e.closest(".visually-hidden")) continue;
        // A line clamp or an ellipsis cuts on purpose, and says so.
        if ((s.overflowX === "hidden" || s.overflowX === "clip") && e.scrollWidth > e.clientWidth + 1 && s.textOverflow !== "ellipsis" && !(s.webkitLineClamp && s.webkitLineClamp !== "none"))
          problems.push({ kind: "text clipped", element: describe(e), text: text(e).slice(0, 80) });
        if (r.right > vw + 0.5) problems.push({ kind: "text past the edge", element: describe(e), text: text(e).slice(0, 80) });
        const disabled = e.closest("button:disabled, fieldset:disabled, [aria-disabled=true]");
        let faded = 1;
        for (let n = e; n && n.nodeType === 1; n = n.parentElement) faded *= Number(getComputedStyle(n).opacity);
        // A thread's own colour (its name) is the thread's, not this surface's.
        const accent = e.closest("[style*='color']");
        if (!disabled && faded > 0.9 && !accent) {
          const value = ratio(e);
          const size = parseFloat(s.fontSize), bold = Number(s.fontWeight) >= 700;
          const need = size >= 24 || (bold && size >= 18.66) ? 3 : 4.5;
          if (value !== undefined && value < need)
            problems.push({ kind: "low contrast", element: describe(e), text: text(e).slice(0, 60), ratio: Math.round(value * 100) / 100, need });
        }
      }
    }
    // A sheet ends above the tab bar, or its last rows sit under it.
    const tabs = document.querySelector(".mobile-tabs");
    const dialog = all("[role=dialog]").at(-1);
    if (dialog && tabs && visible(tabs)) {
      const d = dialog.getBoundingClientRect(), t = tabs.getBoundingClientRect();
      const z = (e) => getComputedStyle(e).zIndex;
      if (d.bottom > t.top + 0.5 && d.right > t.left && d.left < t.right && document.elementFromPoint(d.left + d.width / 2, Math.min(d.bottom, vh) - 2)?.closest(".mobile-tabs"))
        problems.push({ kind: "sheet under the tab bar", dialog: d.toJSON(), tabs: t.toJSON(), scrim: dialog.parentElement.getBoundingClientRect().toJSON(), scrimParent: describe(dialog.parentElement.parentElement), scrimParentBox: dialog.parentElement.parentElement.getBoundingClientRect().toJSON(), maxHeight: getComputedStyle(dialog).maxHeight, z: [z(dialog.parentElement), z(tabs)], vh, inner: innerHeight, root: document.getElementById("root").getBoundingClientRect().toJSON(), bodyHeight: getComputedStyle(document.body).height, viewportVar: document.documentElement.style.getPropertyValue("--viewport-height") });
    }
    if (document.scrollingElement.scrollWidth > vw + 1)
      problems.push({ kind: "page scrolls sideways", width: document.scrollingElement.scrollWidth, viewport: vw });
    return problems;
  };
  const settle = async () => {
    await document.fonts?.ready;
    await new Promise(requestAnimationFrame);
    await new Promise(requestAnimationFrame);
  };
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const tabs = (name) => find(".mobile-tabs button", name);
  return { visible, text, nameOf, all, find, need, point, audit, settle, sleep, ratio, describe, insets, tabs };
})()`;

/**
 * The driver both platforms give: {
 *   platform: "ios" | "android",
 *   evaluate(source, arg): runs `async (arg, lq) => …` (source text) in the app's page,
 *   tap({x, y}): a tap at viewport point (a native touch where the platform has one),
 *   shot(path): a PNG of the whole screen, system bars included,
 *   appearance("light" | "dark"): the OS appearance,
 *   orientation("portrait" | "landscape"), textSize("default" | "largest"),
 *   offline(on): cuts or restores the app's route to the gateway,
 *   owner(method, params): the hub, as its user,
 *   confirmNative(): accepts the native confirmation the next step raises,
 *   hubURL: the gateway URL the app pairs with,
 * }
 */
class Run {
  constructor(driver, fixture, { shots, label }) {
    this.d = driver;
    this.f = fixture;
    this.dir = shots;
    this.label = label || driver.platform;
    this.step = 0;
    this.report = { platform: driver.platform, steps: [], problems: [] };
    if (shots) mkdirSync(shots, { recursive: true });
  }
  async ev(source, arg = {}) {
    return this.d.evaluate(
      `async (arg) => { const lq = ${HELPERS}; return await (${source})(arg, lq); }`,
      arg,
    );
  }
  async until(source, arg = {}, { timeout = 30000, what = source } = {}) {
    const deadline = Date.now() + timeout;
    let last;
    while (Date.now() < deadline) {
      try {
        last = await this.ev(source, arg);
        if (last) return last;
      } catch (error) {
        last = String(error.message || error);
      }
      await delay(250);
    }
    const body = await this.ev("async (a, lq) => lq.text(document.body)").catch(
      () => "",
    );
    throw new Error(
      `Timed out (${timeout} ms): ${what}\nlast: ${JSON.stringify(last)?.slice(0, 500)}\nscreen: ${String(body).slice(0, 2000)}`,
    );
  }
  /** Taps the visible control `selector` named `name` (a string or {re}). */
  async tap(selector, name, root) {
    // The point is taken once the element stops moving, and the tap must reach it: a
    // layout still settling (a rotation, a text size) gets another try.
    for (let attempt = 1; ; attempt++) {
      const p = await this.ev(
        `async (a, lq) => {
          const e = lq.need(a.selector, a.name, a.root);
          lq.point(e);
          let r, last;
          for (let i = 0; i < 20; i++) {
            await lq.settle();
            r = e.getBoundingClientRect();
            if (last && Math.abs(r.top - last.top) < 0.5 && Math.abs(r.left - last.left) < 0.5) break;
            last = r;
          }
          window.__loopTapHit = false;
          e.addEventListener('pointerdown', () => (window.__loopTapHit = true), { once: true, capture: true });
          return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
        }`,
        { selector, name, root },
      );
      await this.d.tap(p);
      await delay(150);
      if (await this.ev("async () => window.__loopTapHit")) return;
      assert(
        attempt < 4,
        `A tap did not reach ${selector} ${JSON.stringify(name)}`,
      );
      await delay(500);
    }
  }
  /** The app's own theme and the OS appearance. */
  async theme(mode) {
    await this.d.appearance(mode);
    await this.ev(
      "async (a, lq) => { localStorage.setItem('twr.theme', a.mode); document.documentElement.dataset.theme = a.mode; await lq.settle(); return true; }",
      { mode },
    );
    await delay(400);
  }
  /** Screens for step `name`: dark then light (back to dark), with an audit of `root`. */
  async shots(
    name,
    {
      root,
      audit = true,
      smallOk,
      transient = [],
      modes = ["dark", "light"],
    } = {},
  ) {
    this.step++;
    const number = String(this.step).padStart(2, "0");
    const entry = { step: number, name, files: [], problems: [] };
    for (const mode of modes) {
      await this.theme(mode);
      await this.ev("async (a, lq) => { await lq.settle(); return true; }");
      const file = join(
        this.dir,
        `${this.label}-${number}-${name}-${mode}.png`,
      );
      await this.d.shot(file);
      entry.files.push(file);
      if (audit) {
        const problems = await this.ev(
          "async (a, lq) => lq.audit(a.root, { smallOk: a.smallOk })",
          { root, smallOk },
        );
        for (const p of problems)
          // What a passing notice covers for its few seconds is noted, not failed.
          if (transient.some((by) => p.by?.includes(by)))
            (entry.notes ||= []).push({ ...p, appearance: mode });
          else entry.problems.push({ ...p, appearance: mode });
      }
    }
    if (modes.at(-1) !== "dark") await this.theme("dark");
    // One problem per element and kind, whichever appearance showed it.
    const seen = new Set();
    entry.problems = entry.problems.filter((p) => {
      const key = `${p.kind} ${p.element || ""} ${p.appearance === "light" && p.kind === "low contrast" ? "light" : ""}`;
      if (seen.has(key)) return false;
      seen.add(key);
      return true;
    });
    this.report.steps.push(entry);
    this.report.problems.push(
      ...entry.problems.map((p) => ({ step: `${number}-${name}`, ...p })),
    );
    console.log(
      "LOOP_QA_SHOT",
      JSON.stringify({ step: `${number}-${name}`, problems: entry.problems }),
    );
  }
  /** Fails on what the screens' audits found. */
  audited() {
    assert.deepEqual(
      this.report.problems,
      [],
      "The loop screens' audit found problems",
    );
  }
  pass(what, detail) {
    this.report.steps.push({ pass: what, detail });
    console.log(
      `PASS: ${this.label} loops: ${what}`,
      detail ? JSON.stringify(detail) : "",
    );
  }
  /** Closes dialogs and drawers, and goes to tab `name`. */
  async tab(name) {
    await this.ev(
      "async (a, lq) => { for (let i = 0; i < 4; i++) { const close = lq.find('[role=dialog] button', 'Close dialog') || lq.find('button', 'Hide sidebar') || lq.find('button', 'Close thread details'); if (!close) break; close.click(); await lq.sleep(250); } return true; }",
    );
    await this.tap(".mobile-tabs button", { re: `^${name}` });
    await delay(300);
  }
}

const THREAD = "loop-worker";

/**
 * The review loop flow on an `operate` device: the offer, the setup card as a sheet, the
 * editor, a running loop on the card and in the inspector, its end, a second run without a
 * card, Settings › Loops, and nothing queued while offline.
 */
export async function loopQA(driver, fixture, options) {
  const run = new Run(driver, fixture, options);
  const { d, f } = { d: driver, f: fixture };
  const owner = (method, params) => d.owner(method, params);
  await run.ev(
    "async () => { window.__loopErrors = []; addEventListener('error', (e) => window.__loopErrors.push(e.message)); return true; }",
  );
  await owner("loop.settings.reset", {}).catch(() => {});
  // A thread whose turn ended with changes.
  await owner("thread.new", { project: "demo", name: THREAD, runtime: "fake" });
  await owner("thread.send", {
    thread: THREAD,
    prompt: "!write lib.rs pub fn f() { todo!() }",
  });
  await run.until(
    "async (a) => { const t = (await window.twr.rpc('thread.get', { thread: a.thread })); const v = t.thread || t; return v.current_turn == null && Number(v.last_turn?.turn) > 0 && Number(v.git?.dirty_files) > 0; }",
    { thread: THREAD },
    { timeout: 60000, what: "loop-worker's turn ends with changes" },
  );

  // 1. The quiet Review loop button on the thread's card (the Threads tab).
  await run.tab("Threads");
  const card = `.thread-card`;
  await run.until(
    "async (a, lq) => !!lq.all('.thread-item').find((i) => lq.text(i).includes(a.thread) && i.querySelector('.loop-offer'))",
    { thread: THREAD },
    { what: "the Review loop button on loop-worker's card" },
  );
  await run.shots("offer-card", { root: ".thread-grid" });
  // ... and on its report in the conversation.
  await run.tap(card, { re: `^${THREAD}` });
  await run.tap("button", "Talk to this thread");
  await run.until(
    "async (a, lq) => !!lq.find('.loop-offer-row .loop-offer', { re: '^Review loop' })",
    {},
    { what: "the Review loop button under the conversation" },
  );
  await run.ev(
    "async () => { document.activeElement?.blur(); await window.Capacitor.Plugins.Keyboard?.hide?.().catch?.(() => {}); return true; }",
  );
  await run.shots("offer-conversation", { root: ".loop-offer-row" });
  run.pass("the Review loop button shows on the card and the report");

  // 2. Tapping it with no saved settings raises the setup card: a row, then a sheet.
  await run.tab("Threads");
  await run.tap(".thread-item .loop-offer", `Review loop: ${THREAD}`);
  await run.until(
    "async (a, lq) => !!lq.find('.loop-setup-row button', 'Review setup')",
    {},
    { what: "the setup card's row in Needs you" },
  );
  await run.shots("setup-row", { root: ".loop-setup-row" });
  // The 600 ms rule: a tap as the sheet opens does nothing; the same tap later answers.
  const early = await run.ev(`async (a, lq) => {
    const open = lq.need('.loop-setup-row button', 'Review setup');
    open.focus();
    open.click();
    let start;
    const t0 = performance.now();
    while (!(start = lq.find('[role=dialog] .attention-loop-setup button', 'Start and save')) && performance.now() - t0 < 3000)
      await new Promise(requestAnimationFrame);
    if (!start) throw new Error('No sheet');
    const shownAt = performance.now();
    const armedAtTap = start.closest('.attention-options').dataset.armed;
    start.click();
    const tappedAfter = Math.round(performance.now() - shownAt);
    await lq.sleep(150);
    const sent = !!lq.find('[role=dialog] .attention-sent');
    let armedAt;
    while (performance.now() - shownAt < 3000) {
      if (start.closest('.attention-options')?.dataset.armed === 'true') { armedAt = performance.now() - shownAt; break; }
      await lq.sleep(20);
    }
    const dialog = document.querySelector('[role=dialog]');
    return { tappedAfter, armedAtTap, sent, armedAfter: Math.round(armedAt), sheet: !!dialog, modal: dialog?.getAttribute('aria-modal'), focusInside: dialog?.contains(document.activeElement), title: lq.text(dialog?.querySelector('h2')) };
  }`);
  assert.equal(early.armedAtTap, "false", JSON.stringify(early));
  assert.equal(early.sent, false, "A tap before 600 ms must not answer");
  assert(early.armedAfter >= 550, `Armed after ${early.armedAfter} ms`);
  assert.equal(early.modal, "true");
  assert.equal(early.focusInside, true, "The sheet takes focus");
  const stillOpen = await owner("loop.list", {});
  assert.equal(stillOpen.length, 0, "No loop starts");
  run.pass(
    "the setup card opens as a sheet; a tap before 600 ms is ignored",
    early,
  );
  const rows = await run.ev(
    "async (a, lq) => lq.all('[role=dialog] .loop-setup-rows dt').map(lq.text)",
  );
  for (const row of ["Profile", "Scope", "Implementer", "Workers", "Reviewer"])
    assert(rows.includes(row), `The setup card shows ${row}: ${rows}`);
  const buttons = await run.ev(
    "async (a, lq) => lq.all('[role=dialog] .attention-options button').map(lq.nameOf)",
  );
  assert.deepEqual(buttons, ["Start and save", "Change…", "Cancel"]);
  await run.shots("setup-sheet", { root: "[role=dialog]" });
  // The sheet scrolls to its last button.
  const last = await run.ev(`async (a, lq) => {
    const cancel = lq.need('[role=dialog] .attention-options button', 'Cancel');
    cancel.scrollIntoView({ block: 'end' });
    await lq.settle();
    const r = cancel.getBoundingClientRect(), vh = visualViewport?.height || innerHeight, safe = lq.insets();
    const hit = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2);
    return { reachable: r.bottom <= vh - safe.bottom + 0.5 && cancel.contains(hit), box: r.toJSON(), vh, safe };
  }`);
  assert(
    last.reachable,
    "The sheet scrolls to Cancel: " + JSON.stringify(last),
  );
  // Focus: the sheet traps Tab and gives focus back to Review setup on Escape, as the
  // app's other sheets do.
  const focus = await run.ev(`async (a, lq) => {
    const dialog = document.querySelector('[role=dialog]');
    const nodes = [...dialog.querySelectorAll('button:not(:disabled), select, input, [tabindex="0"]')].filter(lq.visible);
    nodes.at(-1).focus();
    document.activeElement.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true }));
    const wrapped = document.activeElement === nodes[0];
    document.activeElement.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }));
    await lq.sleep(300);
    const back = document.activeElement;
    return { wrapped, closed: !document.querySelector('[role=dialog]'), returned: lq.nameOf(back) || back.tagName };
  }`);
  assert(focus.wrapped, "Tab wraps inside the sheet: " + JSON.stringify(focus));
  assert(focus.closed, "Escape closes the sheet");
  assert.equal(
    focus.returned,
    "Review setup",
    "Focus goes back to Review setup",
  );
  run.pass("the sheet traps focus and returns it on close", focus);

  // Landscape and the largest text, on the sheet and the editor.
  await run.tap(".loop-setup-row button", "Review setup");
  await run.until(
    "async (a, lq) => !!lq.find('[role=dialog] .attention-loop-setup')",
  );
  await d.orientation("landscape");
  await delay(1500);
  await run.shots("setup-sheet-landscape", {
    root: "[role=dialog]",
    modes: ["dark"],
  });
  await run.tap("[role=dialog] .attention-options button", "Change…");
  await run.until(
    "async (a, lq) => document.querySelectorAll('[role=dialog]').length === 2 && !!lq.find('[role=dialog] .loop-presets')",
  );
  await run.shots("editor-landscape", {
    root: "[role=dialog]",
    modes: ["dark"],
  });
  await run.tap("[role=dialog] button", "Cancel");
  await d.orientation("portrait");
  await delay(1500);
  await d.textSize("largest");
  await run.until(
    "async () => parseFloat(getComputedStyle(document.documentElement).fontSize) >= 24",
    {},
    { what: "the largest text size", timeout: 20000 },
  );
  await run.shots("setup-sheet-largest-text", {
    root: "[role=dialog]",
    modes: ["dark"],
  });
  const largeLast = await run.ev(`async (a, lq) => {
    const cancel = lq.need('[role=dialog] .attention-options button', 'Cancel');
    cancel.scrollIntoView({ block: 'end' });
    await lq.settle();
    const r = cancel.getBoundingClientRect(), vh = visualViewport?.height || innerHeight;
    return r.bottom <= vh + 0.5 && cancel.contains(document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2));
  }`);
  assert(largeLast, "At the largest text the sheet still scrolls to Cancel");
  await run.ev(
    "async (a, lq) => { document.querySelector('[role=dialog] .dialog')?.scrollTo?.(0, 0); document.querySelector('[role=dialog]')?.scrollTo?.(0, 0); return true; }",
  );
  await run.tap("[role=dialog] .attention-options button", "Change…");
  await run.until("async (a, lq) => !!lq.find('[role=dialog] .loop-presets')");
  await run.shots("editor-largest-text", {
    root: "[role=dialog]",
    modes: ["dark"],
  });
  await run.ev(
    `async (a, lq) => { const d = [...document.querySelectorAll('[role=dialog]')].at(-1); for (const e of [d, ...d.querySelectorAll('*')]) if (e.scrollHeight > e.clientHeight + 1 && /auto|scroll/.test(getComputedStyle(e).overflowY)) e.scrollTop = e.scrollHeight; await lq.settle(); return true; }`,
  );
  await run.shots("editor-largest-text-end", {
    root: "[role=dialog]",
    modes: ["dark"],
  });
  await run.tap("[role=dialog] button", "Cancel");
  await d.textSize("default");
  await run.until(
    "async () => parseFloat(getComputedStyle(document.documentElement).fontSize) < 20",
    {},
    { what: "the default text size", timeout: 20000 },
  );

  // 3. Change… opens the editor: presets, pickers, profile, plan check, commit, Save for.
  await run.tap("[role=dialog] .attention-options button", "Change…");
  await run.until("async (a, lq) => !!lq.find('[role=dialog] .loop-presets')");
  const editor = await run.ev(`async (a, lq) => {
    const d = [...document.querySelectorAll('[role=dialog]')].at(-1);
    const presets = lq.all('.loop-presets button', d).map((b) => ({ name: lq.nameOf(b), pressed: b.getAttribute('aria-pressed') }));
    const group = d.querySelector('.loop-presets')?.getAttribute('role');
    const fields = lq.all('label.field > span:first-child, .field > span:first-child, .loop-editor-heading', d).map(lq.text);
    return { presets, group, fields, saveFor: lq.all('.segmented:not(.loop-presets) button', d).map(lq.nameOf), save: lq.all('button', d).map(lq.nameOf) };
  }`);
  console.log("LOOP_QA_EDITOR", JSON.stringify(editor));
  assert.equal(editor.presets.length, 4, "Four presets");
  assert.equal(editor.group, "group");
  for (const field of [
    "Preset",
    "Implementer",
    "Reviewer",
    "Model",
    "Profile",
    "Plan check",
    "Implementation commits",
    "Review commits",
    "Save for",
  ])
    assert(
      editor.fields.some((f) => f.startsWith(field)),
      `The editor has ${field}: ${editor.fields}`,
    );
  await run.shots("editor", { root: "[role=dialog]" });
  // Change the profile and the reviewer's model, save to the card, see them on it.
  const changed = await run.ev(`async (a, lq) => {
    const d = [...document.querySelectorAll('[role=dialog]')].at(-1);
    const set = (select, value) => { Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value').set.call(select, value); select.dispatchEvent(new Event('change', { bubbles: true })); };
    const profile = [...d.querySelectorAll('label.field')].find((l) => lq.text(l.querySelector('span')) === 'Profile').querySelector('select');
    const options = [...profile.options].map((o) => o.value);
    const target = options.find((o) => o !== profile.value && o === 'low') || options.find((o) => o !== profile.value);
    set(profile, target);
    await lq.settle();
    return { profile: target, options };
  }`);
  await run.shots("editor-changed", {
    root: "[role=dialog]",
    modes: ["dark"],
  });
  await run.tap("[role=dialog] button", "Save to the card");
  await run.until(
    "async (a, lq) => document.querySelectorAll('[role=dialog]').length === 1 && lq.all('[role=dialog] .loop-setup-rows div').some((r) => lq.text(r).startsWith('Profile') && lq.text(r).includes(a.profile))",
    { profile: changed.profile },
    { what: "the card shows the changed profile" },
  );
  await run.shots("setup-sheet-changed", { root: "[role=dialog]" });
  run.pass(
    "Change… edits the card's settings and the card shows them",
    changed,
  );

  // 4. Start and save: the loop runs, on the card and in the inspector.
  f.hold(1);
  await delay(700);
  await run.tap("[role=dialog] .attention-options button", "Start and save");
  await run.until(
    "async () => (await window.twr.rpc('loop.list')).length > 0",
    {},
    { what: "the loop started" },
  );
  const waitRuns = async (n, what) => {
    const deadline = Date.now() + 60000;
    while (f.runs() < n && Date.now() < deadline) await delay(200);
    assert(f.runs() >= n, what);
  };
  await waitRuns(1, "the first review run starts");
  await run.tab("Threads");
  await run.until(
    "async (a, lq) => !!lq.all('.thread-item').find((i) => lq.text(i).includes(a.thread) && i.querySelector('.loop-chip'))",
    { thread: THREAD },
    { what: "the loop chip on loop-worker's card" },
  );
  const chip = await run.ev(
    `async (a, lq) => {
    const item = lq.all('.thread-item').find((i) => lq.text(i).includes(a.thread));
    const card = item.querySelector('.thread-card'), chip = item.querySelector('.loop-chip');
    const kids = [...card.children].filter(lq.visible).map((c) => { const r = c.getBoundingClientRect(); return { cls: c.className, top: r.top, bottom: r.bottom, right: r.right, cardRight: card.getBoundingClientRect().right, cardBottom: card.getBoundingClientRect().bottom }; });
    return { line: lq.text(chip), kids, offer: !!item.querySelector('.loop-offer') };
  }`,
    { thread: THREAD },
  );
  assert.match(chip.line, /REVIEW LOOP/);
  assert.equal(chip.offer, false, "No Review loop offer while the loop runs");
  for (const k of chip.kids)
    assert(
      k.bottom <= k.cardBottom + 0.5 && k.right <= k.cardRight + 0.5,
      "The chip keeps the card's rows inside it: " + JSON.stringify(chip.kids),
    );
  await run.shots("chip-card", { root: ".thread-grid" });
  run.pass("the loop chip shows on the card with the loop line", chip.line);
  // A tap on the chip opens the inspector at the Loops section.
  await run.tap(".thread-item .loop-chip");
  await run.until(
    "async (a, lq) => !!lq.find('.inspector-loops .loop-controls button', 'Pause')",
    {},
    { what: "the inspector's loop progress" },
  );
  const controls = await run.ev(
    "async (a, lq) => ({ buttons: lq.all('.inspector-loops button').map(lq.nameOf), phases: lq.all('.loop-phases li').map(lq.text), current: lq.text(document.querySelector('.loop-phases [aria-current=step]')), status: lq.text(document.querySelector('.loop-status')) })",
  );
  for (const name of ["Pause", "Skip to verify", "Stop…"])
    assert(controls.buttons.includes(name), `${name} in ${controls.buttons}`);
  await run.shots("inspector-running", { root: ".inspector" });
  run.pass(
    "the inspector's Loops section shows progress and its controls",
    controls,
  );
  await run.tap(".inspector-loops .loop-controls button", "Pause");
  await run.until(
    "async (a, lq) => !!lq.find('.inspector-loops .loop-controls button', 'Resume')",
    {},
    { what: "paused" },
  );
  await run.shots("inspector-paused", { root: ".inspector", modes: ["dark"] });
  await run.tap(".inspector-loops .loop-controls button", "Resume");
  await run.until(
    "async (a, lq) => !!lq.find('.inspector-loops .loop-controls button', 'Pause')",
    {},
    { what: "resumed" },
  );
  run.pass("Pause and Resume");
  // The round's finding comes back, the thread triages it, round 2 runs.
  f.hold(2);
  f.release(1);
  await waitRuns(2, "the second review run starts");
  await run.until(
    "async (a, lq) => lq.all('.loop-rounds li').some((l) => /m?1/.test(lq.text(l)) && /1 finding/.test(lq.text(l)))",
    {},
    { what: "round 1 with its finding", timeout: 60000 },
  );
  await run.ev(
    "async (a, lq) => { const d = document.querySelector('.loop-rounds details'); if (d) { d.open = true; d.scrollIntoView({ block: 'center' }); } await lq.settle(); return true; }",
  );
  await run.shots("inspector-round", { root: ".inspector" });
  // 5. The loop ends: the report, no chip, the notice of how it ended.
  f.releaseAll();
  await run.until(
    "async () => { return !(await window.twr.rpc('loop.list')).length; }",
    {},
    { what: "the loop ends", timeout: 120000 },
  );
  await run.until(
    "async (a, lq) => !!lq.find('.inspector-loops button', 'Open report')",
    {},
    { what: "Open report", timeout: 30000 },
  );
  await run.tap(".inspector-loops button", "Open report");
  const ended = await run.ev(
    "async (a, lq) => ({ last: lq.text(document.querySelector('.loop-last')), report: lq.text(document.querySelector('.inspector-report')), path: lq.text(document.querySelector('.loop-report .loop-path code')) })",
  );
  assert.match(ended.path, /report\.md$/);
  assert.match(ended.report, /review loop \w+/);
  await run.shots("inspector-ended", { root: ".inspector" });
  await run.tab("Threads");
  const after = await run.ev(
    "async (a, lq) => { const i = lq.all('.thread-item').find((i) => lq.text(i).includes(a.thread)); return { chip: !!i.querySelector('.loop-chip'), offer: !!i.querySelector('.loop-offer'), text: lq.text(i) }; }",
    { thread: THREAD },
  );
  assert.equal(after.chip, false, "The chip goes away");
  assert.equal(after.offer, false, "The reviewed turn offers no second review");
  await run.shots("ended-card", { root: ".thread-grid" });
  // How it ended, in the conversation's Latest thread reports (the phone gets no system
  // notification for it: mobile push covers requests only, without content).
  await run.tab("More");
  await run.tap("button.workspace-orchestrator", { re: "Orchestrator" });
  await run.tap("details.reports > summary", { re: "^Latest thread reports" });
  const notice = await run.until(
    "async (a, lq) => { const row = lq.all('details.reports .report-row').find((r) => lq.text(r).includes(a.thread)); if (!row || !/review loop \\w+ after/.test(lq.text(row))) return false; row.scrollIntoView({ block: 'center' }); await lq.settle(); return lq.text(row); }",
    { thread: THREAD },
    { what: "Latest thread reports say how the loop ended", timeout: 30000 },
  );
  await run.shots("ended-notice", { root: "details.reports" });
  run.pass("the loop ends: the report, no chip, how it ended in the reports", {
    ...ended,
    after,
    notice,
  });

  // 6. A second run starts on the saved settings, with no card.
  const nextRun = f.runs() + 1;
  f.hold(nextRun);
  await run.tab("Threads");
  await run.tap(".thread-card", { re: `^${THREAD}` });
  await run.tap(".inspector-loops > button", "Review loop");
  await run.until(
    "async () => { return (await window.twr.rpc('loop.list')).length > 0; }",
    {},
    { what: "the second loop starts" },
  );
  const attention = await owner("attention.list", {}).catch(() => undefined);
  const setupCards = JSON.stringify(attention || {}).includes("loop_setup");
  assert.equal(setupCards, false, "No setup card on the second run");
  await run.until(
    "async (a, lq) => !!lq.find('.inspector-loops .loop-controls button', 'Stop…')",
    {},
    { what: "the second loop's controls" },
  );
  // The "started a review loop" notice covers the inspector's close button until it goes
  // (6 s) or is dismissed: noted for the reviewer, not failed.
  await run.shots("second-run", {
    root: ".inspector",
    transient: ["Dismiss notice"],
  });
  // Stop asks first, then stops.
  await run.tap(".inspector-loops .loop-controls button", "Stop…");
  await run.until(
    "async (a, lq) => !!lq.find('[role=dialog] button', 'Stop loop')",
    {},
    { what: "Stop's confirmation" },
  );
  await run.shots("stop-confirm", { root: "[role=dialog]" });
  await run.tap("[role=dialog] button", "Stop loop");
  await run.until(
    "async () => { return !(await window.twr.rpc('loop.list')).length; }",
    {},
    { what: "the second loop stops", timeout: 60000 },
  );
  f.releaseAll();
  run.pass("a second run starts with no card; Stop asks, then stops");

  // 7. Settings › Loops opens the editor.
  await openSettingsLoops(run);
  await run.shots("settings-loops", {
    root: ".settings-section:has(.loop-editor)",
  });
  const settings = await run.ev(
    "async (a, lq) => { const s = lq.all('.settings-section').find((s) => lq.text(s.querySelector('h2')) === 'Loops'); return { presets: lq.all('.loop-presets button', s).length, save: !!lq.find('button', 'Save settings', s), disabled: s.querySelector('fieldset.loop-editor')?.disabled }; }",
  );
  assert.equal(settings.presets, 4);
  assert.equal(settings.save, true);
  assert.equal(settings.disabled, false);
  run.pass("Settings › Loops opens the editor", settings);

  // 8. Offline: no start, no answer, nothing in the outbox.
  await owner("loop.request", { thread: THREAD, kind: "review", setup: true });
  await run.tab("Threads");
  await run.tap(".thread-card", { re: `^${THREAD}` });
  await run.until(
    "async (a, lq) => !!lq.find('.inspector .loop-setup-row button, .inspector .attention-loop-setup button', { re: '^(Review setup|Start and save)$' })",
    {},
    { what: "the raised setup card in the inspector" },
  );
  await d.offline(true);
  await run.until(
    "async (a, lq) => !!lq.find('.inspector-loops > button', 'Review loop')?.disabled",
    {},
    { what: "loop actions off while offline", timeout: 60000 },
  );
  const offline = await run.ev(`async (a, lq) => {
    const review = lq.find('.inspector-loops > button', 'Review loop');
    review.click();
    const open = lq.find('.inspector .loop-setup-row button', 'Review setup');
    if (open) { open.click(); await lq.sleep(800); }
    const start = lq.find('[role=dialog] .attention-options button, .inspector .attention-options button', 'Start and save');
    start?.click();
    await lq.sleep(800);
    const journal = await window.Capacitor?.Plugins?.TwrOffline?.list?.().catch(() => undefined);
    return { review: { disabled: review.disabled, why: review.title }, start: start ? { disabled: start.disabled } : null, outbox: lq.text(document.querySelector('.outbox')), journal };
  }`);
  await run.shots("offline", { root: "[role=dialog], .inspector" });
  await d.offline(false);
  await run.until(
    "async () => { try { await window.twr.rpc('hub.info'); return true; } catch { return false; } }",
    {},
    { what: "back online", timeout: 60000 },
  );
  await delay(3000);
  const afterOffline = await owner("loop.list", {});
  assert.equal(afterOffline.length, 0, "Nothing started from offline taps");
  assert.equal(offline.review.disabled, true);
  assert.equal(offline.start?.disabled ?? true, true);
  assert(
    !/review|loop/i.test(offline.outbox || ""),
    "Nothing in the outbox: " + offline.outbox,
  );
  run.pass(
    "offline: Review loop and the card are off, nothing queued or sent",
    offline,
  );
  // Cancel the card (online, armed).
  await run.ev(
    "async (a, lq) => { lq.find('.inspector .loop-setup-row button', 'Review setup')?.click(); await lq.sleep(900); return true; }",
  );
  await run.tap(
    "[role=dialog] .attention-options button, .inspector .attention-options button",
    "Cancel",
  );
  await run.until(
    "async (a, lq) => !lq.find('.attention-loop-setup')",
    {},
    { what: "the card goes on Cancel" },
  );
  const errors = await run.ev("async () => window.__loopErrors");
  assert.deepEqual(errors, [], "No page errors");
  // Leave the app as found: no details open, on the conversation.
  await run.tab("Chat");
  run.audited();
  return run.report;
}

/** Pairs the app on its pairing form (the onboarding's Pair a hub). */
export async function pairInApp(driver, { url, code, name }) {
  const run = new Run(driver, null, {});
  await run.until(
    "async (a, lq) => { const b = lq.find('button', { re: '^Pair a hub' }); if (b) b.click(); return !!document.querySelector('input[name=code]'); }",
    {},
    { what: "the pairing form" },
  );
  await run.ev(
    `async (a) => {
      const values = { hubName: a.name, url: a.url, code: a.code, name: a.name };
      for (const [key, value] of Object.entries(values)) {
        const input = document.querySelector('input[name="' + key + '"]');
        Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(input, value);
        input.dispatchEvent(new Event('input', { bubbles: true }));
      }
      document.activeElement?.blur();
      document.querySelector('.pair-form').requestSubmit();
      return true;
    }`,
    { url, code, name },
  );
  await run.until(
    "async () => { try { return (await window.twr.rpc('hub.info')).identity.startsWith('user:device:'); } catch { return false; } }",
    {},
    { what: "the paired hub answers", timeout: 120000 },
  );
}

/** Revokes the app's device on its hub (Remote hub settings › Revoke this device), which
 * asks first: `driver.nativeConfirm(click)` accepts that native confirmation. */
export async function revokeInApp(driver) {
  const run = new Run(driver, null, {});
  await run.tab("Chat");
  await run.ev(
    "async (a, lq) => { lq.need('button', { re: '^Remote hub settings:' }).click(); return true; }",
  );
  await run.until("async (a, lq) => !!lq.find('button', 'Revoke this device')");
  await driver.nativeConfirm(() =>
    run.ev(
      "async (a, lq) => { lq.need('button', 'Revoke this device').click(); return true; }",
    ),
  );
  await run.until(
    "async () => !!document.querySelector('.mobile-onboarding')",
    {},
    { what: "the hub list after Revoke", timeout: 60000 },
  );
}

async function openSettingsLoops(run) {
  await run.tab("More");
  await run.tap("#twr-sidebar button, .sidebar button", { re: "^Settings" });
  await run.until(
    "async (a, lq) => !!lq.all('.settings-section h2').find((h) => lq.text(h) === 'Loops')",
    {},
    { what: "Settings › Loops" },
  );
  // Its editor reads the hub first ("Loading loop settings…").
  await run.until(
    "async (a, lq) => { const s = lq.all('.settings-section').find((s) => lq.text(s.querySelector('h2')) === 'Loops'); return !!s?.querySelector('fieldset.loop-editor'); }",
    {},
    { what: "the Settings › Loops editor" },
  );
  await run.ev(
    "async (a, lq) => { lq.all('.settings-section h2').find((h) => lq.text(h) === 'Loops').scrollIntoView({ block: 'start' }); await lq.settle(); return true; }",
  );
}

/**
 * A device paired with only `read`: it sees the loop line and the report, and cannot start
 * a loop, answer the setup card or save settings.
 */
export async function loopReadOnlyQA(driver, fixture, options) {
  const run = new Run(driver, fixture, options);
  run.step = options.step || 40;
  const owner = (method, params) => driver.owner(method, params);
  // A setup card the user raised elsewhere.
  await owner("loop.request", { thread: THREAD, kind: "review", setup: true });
  await run.tab("Needs you");
  await run.until(
    "async (a, lq) => !!lq.find('.loop-setup-row button', 'Review setup')",
    {},
    { what: "the setup card on a read device" },
  );
  await run.tap(".loop-setup-row button", "Review setup");
  await run.until(
    "async (a, lq) => !!lq.find('[role=dialog] .attention-loop-setup')",
  );
  await delay(800);
  const card = await run.ev(
    "async (a, lq) => ({ buttons: lq.all('[role=dialog] .attention-options button').map((b) => ({ name: lq.nameOf(b), disabled: b.disabled })), why: lq.text(document.querySelector('[role=dialog] .loop-readonly')) })",
  );
  assert(
    card.buttons.length === 3 && card.buttons.every((b) => b.disabled),
    "Every card button is off: " + JSON.stringify(card),
  );
  await run.shots("read-setup-card", { root: "[role=dialog]" });
  await run.ev(
    "async (a, lq) => { lq.need('[role=dialog] button', 'Close dialog').click(); return true; }",
  );
  const items = await owner("attention.list", {});
  const open = items.find((i) => JSON.stringify(i).includes("loop_setup"));
  if (open)
    await owner("attention.respond", {
      attn_id: open.attn_id,
      revision: open.revision,
      answer: "cancel",
    });
  // A running loop: the chip and the progress, controls off.
  const nextRun = fixture.runs() + 1;
  fixture.hold(nextRun);
  await owner("loop.request", { thread: THREAD, kind: "review" });
  await run.tab("Threads");
  await run.until(
    "async (a, lq) => !!lq.all('.thread-item').find((i) => lq.text(i).includes(a.thread) && i.querySelector('.loop-chip'))",
    { thread: THREAD },
    { what: "the loop chip on a read device", timeout: 60000 },
  );
  await run.shots("read-chip-card", { root: ".thread-grid" });
  await run.tap(".thread-item .loop-chip");
  await run.until(
    "async (a, lq) => !!lq.find('.inspector-loops .loop-controls button', 'Pause')",
  );
  const controls = await run.ev(
    "async (a, lq) => lq.all('.inspector-loops .loop-controls button').map((b) => ({ name: lq.nameOf(b), disabled: b.disabled, why: b.title }))",
  );
  assert(
    controls.every((c) => c.disabled),
    JSON.stringify(controls),
  );
  await run.shots("read-inspector", { root: ".inspector" });
  // Released, it runs to its end and writes its report.
  fixture.releaseAll();
  await run.until(
    "async (a, lq) => !lq.find('.inspector-loops .loop-controls') && !!lq.find('.inspector-loops button', 'Open report')",
    {},
    { what: "the report on a read device", timeout: 60000 },
  );
  const review = await run.ev(
    "async (a, lq) => { const b = lq.find('.inspector-loops > button', 'Review loop'); return { disabled: b?.disabled, why: b?.title, report: lq.text(document.querySelector('.inspector-report')) }; }",
  );
  assert.equal(review.disabled, true, "A read device cannot start a loop");
  assert.match(review.report, /review loop/);
  await run.shots("read-report", { root: ".inspector" });
  // Settings › Loops shows the settings and saves nothing.
  await openSettingsLoops(run);
  const settings = await run.ev(
    "async (a, lq) => { const s = lq.all('.settings-section').find((s) => lq.text(s.querySelector('h2')) === 'Loops'); return { save: !!lq.find('button', 'Save settings', s), disabled: s.querySelector('fieldset.loop-editor')?.disabled }; }",
  );
  assert.equal(settings.save, false, "No Save on a read device");
  assert.equal(settings.disabled, true);
  await run.shots("read-settings-loops", { root: ".settings-section" });
  run.pass(
    "a read device sees the loop line and the report and changes nothing",
    { card, controls, review, settings },
  );
  run.audited();
  return run.report;
}

const calibrations = new WeakMap();
/**
 * A real touch on Android at viewport point {x, y}: `adb shell input tap` in screen pixels.
 * The WebView's place on the screen is measured once per viewport size, by a tap that the
 * page swallows before the app sees it.
 */
export async function adbTap(page, adbRun, { x, y }) {
  const size = await page.evaluate(() => `${innerWidth}x${innerHeight}`);
  let calibration = calibrations.get(page);
  if (calibration?.size !== size) {
    const display = adbRun(["shell", "wm", "size"])
      .match(/(\d+)x(\d+)\s*$/m)
      .slice(1)
      .map(Number);
    const rotated = await page.evaluate(() => innerWidth > innerHeight);
    const [width, height] = rotated
      ? [Math.max(...display), Math.min(...display)]
      : [Math.min(...display), Math.max(...display)];
    await page.evaluate(() => {
      window.__loopCalibration = new Promise((resolve) => {
        const swallow = (event) => {
          event.preventDefault();
          event.stopImmediatePropagation();
        };
        const types = [
          "pointerup",
          "mousedown",
          "mouseup",
          "click",
          "touchstart",
          "touchend",
        ];
        for (const type of types)
          addEventListener(type, swallow, { capture: true });
        addEventListener(
          "pointerdown",
          (event) => {
            swallow(event);
            setTimeout(() => {
              for (const type of types)
                removeEventListener(type, swallow, { capture: true });
            }, 600);
            resolve({
              x: event.clientX,
              y: event.clientY,
              dpr: devicePixelRatio,
            });
          },
          { capture: true, once: true },
        );
      });
    });
    const sx = Math.round(width / 2),
      sy = Math.round(height / 2);
    adbRun(["shell", "input", "tap", String(sx), String(sy)]);
    const seen = await page.evaluate(() => window.__loopCalibration);
    await delay(700);
    calibration = {
      size,
      dpr: seen.dpr,
      left: sx - seen.x * seen.dpr,
      top: sy - seen.y * seen.dpr,
    };
    calibrations.set(page, calibration);
  }
  adbRun([
    "shell",
    "input",
    "tap",
    String(Math.round(calibration.left + x * calibration.dpr)),
    String(Math.round(calibration.top + y * calibration.dpr)),
  ]);
}

/** A tap on iOS: the events a touch gives, at the viewport point, from the page itself
 * (an app-hosted XCTest cannot synthesize touches). */
export const IOS_TAP = `async (p) => {
  const e = document.elementFromPoint(p.x, p.y);
  if (!e) return false;
  const o = { bubbles: true, cancelable: true, composed: true, clientX: p.x, clientY: p.y, pointerType: "touch", isPrimary: true, button: 0 };
  e.dispatchEvent(new PointerEvent("pointerdown", o));
  e.dispatchEvent(new PointerEvent("pointerup", o));
  e.dispatchEvent(new MouseEvent("click", o));
  return true;
}`;

/** Accepts the next native confirmation (\`@capacitor/dialog\` confirm) from the page. */
export const IOS_ACCEPT_CONFIRM = `async () => {
  const original = window.Capacitor.nativePromise;
  window.Capacitor.nativePromise = (plugin, method, options) => {
    if (plugin === "Dialog" && method === "confirm") {
      window.Capacitor.nativePromise = original;
      return Promise.resolve({ value: true });
    }
    return original.call(window.Capacitor, plugin, method, options);
  };
  return true;
}`;
