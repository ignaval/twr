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
 * `surfacesQA` audits the same way the surfaces every screen shares: the thread cards, a
 * thread's details, the conversation's header and the whole composer.
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
import { join, resolve } from "node:path";
import { execFileSync, spawn } from "node:child_process";
import { createServer } from "node:net";

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
const PLAN =
  'Plan: a CSV export on the reports page.\n```twr-plan\n{"contract_risk": false, "reason": "UI only: a new button"}\n```\n';
const IMPLEMENT =
  '!write feature.rs pub fn export_csv() {}\nImplemented.\n```twr-implement\n{"status": "done", "gates": {"ran": ["true"], "result": "pass"}}\n```\n';
const TEMPLATES = {
  goal: "Gates are green.\n```twr-goal\nHarden the change against bad input.\n```\n",
  triage:
    '{{findings}}\nTriaged.\n```twr-triage\n{"items": [{"id": "{{finding_ids}}", "verdict": "dismissed", "reason": "empty input never reaches it"}], "gates": {"ran": ["true"], "result": "pass"}}\n```\n',
  report: "The loop reviewed the change; nothing is left open.\n",
  // An implementation loop's steps: the plan, the implement turn that writes a file, and
  // the report turn, which sleeps while the QA commits the work for it (the fake runtime
  // runs no git), then names a commit (the hub checks the repository, not the sha). The
  // commit's wording comes before the block: the fake echoes its indented example as a
  // fence, and the hub reads the last one.
  plan: PLAN,
  implement: IMPLEMENT,
  "impl-report":
    '!sleep 8000\nBuilt the CSV export.\n{{commit}}\n```twr-commit\n{"commits": [{"repo": "demo", "sha": "abc1234"}]}\n```\n',
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
    /** Replaces template `name` (the hub reads it again for each prompt). */
    template: (name, text) =>
      writeFileSync(join(templates, `${name}.md`), text),
    /** Commits everything in `worktree`, as the thread would for the loop's report turn. */
    commit(worktree) {
      const git = (...args) =>
        execFileSync(
          "git",
          [
            "-C",
            worktree,
            "-c",
            "user.name=QA",
            "-c",
            "user.email=qa@example.invalid",
            ...args,
          ],
          { stdio: "ignore" },
        );
      git("add", "-A");
      git("commit", "-q", "-m", "Add the CSV export");
    },
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
        if (e.closest(".visually-hidden, .sr-only")) continue;
        // A line clamp or an ellipsis cuts on purpose, and says so.
        if ((s.overflowX === "hidden" || s.overflowX === "clip") && e.scrollWidth > e.clientWidth + 1 && s.textOverflow !== "ellipsis" && !(s.webkitLineClamp && s.webkitLineClamp !== "none"))
          problems.push({ kind: "text clipped", element: describe(e), text: text(e).slice(0, 80) });
        if (r.right > vw + 0.5) problems.push({ kind: "text past the edge", element: describe(e), text: text(e).slice(0, 80) });
        const disabled = e.closest("button:disabled, fieldset:disabled, [aria-disabled=true]");
        let faded = 1;
        for (let n = e; n && n.nodeType === 1; n = n.parentElement) faded *= Number(getComputedStyle(n).opacity);
        // A thread's name in its colour is text like any other (its ink, shared/threadColor.ts).
        if (!disabled && faded > 0.9) {
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
  for (const row of ["Profile", "Scope", "Implementer", "Reviewer"])
    assert(rows.includes(row), `The setup card shows ${row}: ${rows}`);
  // The fake runtime runs no subagents: the hub leaves the Workers row out.
  assert(
    !rows.includes("Workers"),
    `No Workers row for a fake thread: ${rows}`,
  );
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
  // The report's text in a sheet (`loop.report`), the path under it.
  await run.until(
    "async (a, lq) => !!lq.find('.loop-report-body .markdown, .loop-report-body p')",
    {},
    { what: "the report's text", timeout: 30000 },
  );
  const ended = await run.ev(
    "async (a, lq) => ({ last: lq.text(document.querySelector('.loop-last')), report: lq.text(document.querySelector('.inspector-report')), text: lq.text(document.querySelector('.loop-report-body')), path: lq.text(document.querySelector('.loop-report-footer .loop-path code')) })",
  );
  assert.match(ended.path, /report\.md$/);
  assert.match(ended.report, /review loop \w+/);
  assert.match(ended.text, /review loop/i);
  await run.shots("report-sheet");
  await run.tap("button[aria-label='Close dialog']");
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
  await run.tap(".inspector-loops .loop-starts button", "Review loop");
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
  // The "started a review loop" notice shows at the top of the inspector, in its flow (on
  // a phone a toast would cover the inspector's close button); it goes after 6 s.
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
    "async (a, lq) => !!lq.find('.inspector-loops .loop-starts button', 'Review loop')?.disabled",
    {},
    { what: "loop actions off while offline", timeout: 60000 },
  );
  const offline = await run.ev(`async (a, lq) => {
    const review = lq.find('.inspector-loops .loop-starts button', 'Review loop');
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
  await implementQA(run, driver, fixture);
  // Leave the app as found: no details open, on the conversation.
  await run.tab("Chat");
  run.audited();
  return run.report;
}

const BUILDER = "loop-builder";
const TASK = "add a CSV export to the reports page";

/** Waits on the host for `check()` (which may be async) to be truthy, and returns it. */
async function poll(check, what, timeout = 60000) {
  const deadline = Date.now() + timeout;
  let last;
  for (;;) {
    try {
      last = await check();
      if (last) return last;
    } catch (error) {
      last = String(error?.message || error);
    }
    if (Date.now() > deadline)
      throw new Error(`Timed out (${timeout} ms): ${what}; last: ${last}`);
    await delay(250);
  }
}
/** A thread as the hub shows it (`thread.get`). */
async function threadView(driver, name) {
  const got = await driver.owner("thread.get", { thread: name });
  return got.thread || got;
}
/** `name`'s running loop (`loop.list`), or undefined. */
async function runningLoop(driver, name) {
  return ((await driver.owner("loop.list", {})) || []).find(
    (l) => l.thread_name === name || l.thread === name,
  );
}
/** Types `text` into the conversation's composer, then puts the keyboard away. */
async function typeTask(run, name, text) {
  await run.ev(
    `async (a, lq) => {
      const box = lq.need('textarea', 'Message ' + a.name + ' directly');
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set.call(box, a.text);
      box.dispatchEvent(new Event('input', { bubbles: true }));
      box.blur();
      await window.Capacitor?.Plugins?.Keyboard?.hide?.().catch?.(() => {});
      await lq.sleep(300);
      return true;
    }`,
    { name, text },
  );
}
/** Opens `name`'s conversation from the Threads tab. */
async function openConversation(run, name) {
  await run.tab("Threads");
  await run.until(
    "async (a, lq) => !!lq.find('.thread-card', { re: '^' + a.name })",
    { name },
    { what: `${name}'s card` },
  );
  await run.tap(".thread-card", { re: `^${name}` });
  await run.tap("button", "Talk to this thread");
  await run.until(
    "async (a, lq) => !!lq.find('textarea', 'Message ' + a.name + ' directly')",
    { name },
    { what: `${name}'s composer` },
  );
}
/** The ↻ button beside Send, then its choice: what the choice says. */
async function openSendAsLoop(run, name) {
  await run.tap(".send-loop-button", `Send as implementation loop to ${name}…`);
  return run.until(
    `async (a, lq) => {
      const opener = lq.find('.send-loop-button', 'Send as implementation loop to ' + a.name + '…');
      const choice = lq.find('.send-loop-panel button', 'Send as implementation loop');
      if (!choice) return false;
      return { expanded: opener?.getAttribute('aria-expanded'), disabled: choice.disabled, what: lq.text(document.getElementById(choice.getAttribute('aria-describedby'))) };
    }`,
    { name },
    { what: "Send as implementation loop" },
  );
}
/** Whether the open sheet's last answer can be scrolled to and tapped (not under the
 * home indicator, the keyboard or the edge). */
const LAST_REACHABLE = `async (a, lq) => {
  const last = lq.all('[role=dialog] ' + a.selector).at(-1);
  if (!last) return { reachable: false, why: 'none' };
  last.scrollIntoView({ block: 'end' });
  await lq.settle();
  const r = last.getBoundingClientRect(), vh = visualViewport?.height || innerHeight, safe = lq.insets();
  const hit = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2);
  return { reachable: r.bottom <= vh - safe.bottom + 0.5 && r.top >= safe.top - 0.5 && last.contains(hit), name: lq.nameOf(last), box: r.toJSON(), vh, safe };
}`;
/** The phone's 600 ms rule on a setup card's sheet: opens it from its row with an
 * immediate tap on Start and save, which must answer nothing. */
const EARLY_TAP = `async (a, lq) => {
  const open = lq.need('.loop-setup-row button', 'Review setup', a.root);
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
  await lq.sleep(150);
  const sent = !!lq.find('[role=dialog] .attention-sent');
  let armedAt;
  while (performance.now() - shownAt < 3000) {
    if (start.closest('.attention-options')?.dataset.armed === 'true') { armedAt = performance.now() - shownAt; break; }
    await lq.sleep(20);
  }
  const dialog = [...document.querySelectorAll('[role=dialog]')].at(-1);
  return {
    armedAtTap, sent, armedAfter: Math.round(armedAt),
    modal: dialog?.getAttribute('aria-modal'),
    title: lq.text(dialog?.querySelector('h2')),
    buttons: lq.all('.attention-options button', dialog).map(lq.nameOf),
    task: lq.text(dialog?.querySelector('.loop-setup-task')),
    rows: lq.all('.loop-setup-rows dt', dialog).map(lq.text),
  };
}`;

/**
 * Implementation loops on an `operate` phone: the ↻ button beside Send opens Send as
 * implementation loop; with no saved settings its card holds the task, as a sheet with four
 * answers under the 600 ms rule; Run without a loop sends the task as a message; Start and
 * save runs it (the chip, the inspector's plan, implement and rounds, the end with its
 * commit); Open report shows the report's text as a sheet that scrolls; the New thread
 * form's switch starts one; offline nothing is started or queued.
 */
async function implementQA(run, d, f) {
  const owner = (method, params) => d.owner(method, params);
  await owner("loop.settings.reset", {});
  assert.equal(
    (await owner("loop.settings.get", {})).settings ?? null,
    null,
    "No saved settings",
  );
  await owner("thread.new", {
    project: "demo",
    name: BUILDER,
    runtime: "fake",
  });
  await openConversation(run, BUILDER);

  // 1. The ↻ button beside Send opens Send as implementation loop.
  await typeTask(run, BUILDER, TASK);
  const choice = await openSendAsLoop(run, BUILDER);
  assert.equal(choice.expanded, "true");
  assert.equal(choice.disabled, false);
  assert.match(choice.what, /plans your message as a task/);
  await run.shots("implement-choice", { root: ".send-as-loop" });
  run.pass(
    "the ↻ button beside Send opens Send as implementation loop",
    choice,
  );

  // 2. No saved settings: the card holds the task, a row that opens as a sheet with four
  //    answers in order, under the 600 ms rule.
  await run.tap(".send-loop-panel button", "Send as implementation loop");
  await run.until(
    "async (a, lq) => !!lq.find('.loop-setup-row button', 'Review setup')",
    {},
    { what: "the implementation loop's setup row" },
  );
  const left = await run.ev(
    "async (a, lq) => lq.need('textarea', 'Message ' + a.name + ' directly').value",
    { name: BUILDER },
  );
  assert.equal(left, "", "The task left the composer");
  await run.shots("implement-setup-row", {
    root: ".loop-setup-row",
    transient: ["Dismiss notice"],
  });
  const early = await run.ev(EARLY_TAP, {});
  console.log("LOOP_QA_IMPLEMENT_SHEET", JSON.stringify(early));
  assert.equal(early.armedAtTap, "false", JSON.stringify(early));
  assert.equal(early.sent, false, "A tap before 600 ms must not answer");
  assert(early.armedAfter >= 550, `Armed after ${early.armedAfter} ms`);
  assert.equal(early.modal, "true");
  assert.equal(early.title, `Implementation loop setup · ${BUILDER}`);
  assert.deepEqual(early.buttons, [
    "Start and save",
    "Change…",
    "Run without a loop",
    "Cancel",
  ]);
  assert.equal(early.task, TASK, "The sheet shows the task");
  for (const row of ["Task", "Plan check", "Commit"])
    assert(early.rows.includes(row), `The card has ${row}: ${early.rows}`);
  assert.equal(await runningLoop(d, BUILDER), undefined, "Nothing started");
  run.pass(
    "with no saved settings the card holds the task: a sheet with its four answers, a tap before 600 ms ignored",
    early,
  );
  await run.shots("implement-setup-sheet", { root: "[role=dialog]" });
  const reach = await run.ev(LAST_REACHABLE, {
    selector: ".attention-options button",
  });
  assert(
    reach.reachable,
    "The sheet scrolls to Cancel: " + JSON.stringify(reach),
  );
  await d.orientation("landscape");
  await delay(1500);
  await run.shots("implement-setup-sheet-landscape", {
    root: "[role=dialog]",
    modes: ["dark"],
  });
  const reachSideways = await run.ev(LAST_REACHABLE, {
    selector: ".attention-options button",
  });
  assert(
    reachSideways.reachable,
    "Sideways the sheet scrolls to Cancel: " + JSON.stringify(reachSideways),
  );
  await d.orientation("portrait");
  await delay(1500);
  await d.textSize("largest");
  await run.until(
    "async () => parseFloat(getComputedStyle(document.documentElement).fontSize) >= 24",
    {},
    { what: "the largest text size", timeout: 20000 },
  );
  await run.shots("implement-setup-sheet-largest-text", {
    root: "[role=dialog]",
    modes: ["dark"],
  });
  const reachLarge = await run.ev(LAST_REACHABLE, {
    selector: ".attention-options button",
  });
  assert(
    reachLarge.reachable,
    "At the largest text the sheet scrolls to Cancel: " +
      JSON.stringify(reachLarge),
  );
  await d.textSize("default");
  await run.until(
    "async () => parseFloat(getComputedStyle(document.documentElement).fontSize) < 20",
    {},
    { what: "the default text size", timeout: 20000 },
  );

  // 3. Run without a loop: the task goes to the thread as an ordinary message.
  // A rotation or a text size re-lays the sheet out: its answers arm again from zero.
  await run.until(
    "async (a, lq) => lq.find('[role=dialog] .attention-options')?.dataset.armed === 'true'",
    {},
    { what: "the sheet's answers arm" },
  );
  await run.tap(
    "[role=dialog] .attention-options button",
    "Run without a loop",
  );
  await run.until(
    "async (a, lq) => !lq.find('.attention-loop-setup') && !document.querySelector('[role=dialog]')",
    {},
    { what: "the card goes on Run without a loop" },
  );
  await run.until(
    `async (a) => {
      const got = await window.twr.rpc('thread.transcript', { thread: a.name });
      return (got.events || got).some((e) => e.kind === 'agent.text' && e.data?.final && e.data.text?.includes(a.task));
    }`,
    { name: BUILDER, task: TASK },
    { what: "the task sent as an ordinary message", timeout: 60000 },
  );
  assert.equal(await runningLoop(d, BUILDER), undefined, "No loop starts");
  assert.equal(
    (await owner("loop.settings.get", {})).settings ?? null,
    null,
    "Run without a loop saves nothing",
  );
  await run.ev(
    "async () => { document.activeElement?.blur(); await window.Capacitor?.Plugins?.Keyboard?.hide?.().catch?.(() => {}); return true; }",
  );
  await run.shots("implement-run-without", { root: ".transcript-virtual" });
  run.pass("Run without a loop sends the task as an ordinary message");

  // 4. Again, with Start and save. The plan and implement turns sleep first, to look at
  //    each; the first review run is held.
  await poll(
    async () => (await threadView(d, BUILDER)).current_turn == null,
    "loop-builder's turn ends",
  );
  const first = f.runs() + 1;
  f.hold(first);
  f.template("plan", "!sleep 30000\n" + PLAN);
  f.template("implement", "!sleep 30000\n" + IMPLEMENT);
  await typeTask(run, BUILDER, TASK);
  await openSendAsLoop(run, BUILDER);
  await run.tap(".send-loop-panel button", "Send as implementation loop");
  await run.until(
    "async (a, lq) => !!lq.find('.loop-setup-row button', 'Review setup')",
    {},
    { what: "the setup row again" },
  );
  await run.tap(".loop-setup-row button", "Review setup");
  await run.until(
    "async (a, lq) => lq.find('[role=dialog] .attention-options')?.dataset.armed === 'true'",
    {},
    { what: "the sheet's answers arm" },
  );
  await run.tap("[role=dialog] .attention-options button", "Start and save");
  await poll(
    async () => (await runningLoop(d, BUILDER))?.phase === "plan",
    "the implementation loop plans",
  );
  await run.tab("Threads");
  await run.until(
    "async (a, lq) => { const i = lq.all('.thread-item').find((i) => lq.text(i).includes(a.name)); return lq.text(i?.querySelector('.loop-chip .loop-badge')) === 'IMPLEMENT LOOP'; }",
    { name: BUILDER },
    { what: "the IMPLEMENT LOOP chip on loop-builder's card" },
  );
  await run.shots("implement-chip", { root: ".thread-grid" });
  await run.tap(".thread-item .loop-chip", { re: "^IMPLEMENT LOOP" });
  const phase = (name) =>
    run.until(
      "async (a, lq) => lq.text(document.querySelector('.inspector-loops .loop-phases [aria-current=step]')) === a.name",
      { name },
      { what: `the inspector's current phase: ${name}`, timeout: 60000 },
    );
  await phase("plan");
  await run.shots("implement-plan", { root: ".inspector" });
  await poll(
    async () => (await runningLoop(d, BUILDER))?.phase === "implement",
    "the implementation loop implements",
  );
  // The implement prompt is out: the templates go back to answering at once.
  f.template("plan", PLAN);
  f.template("implement", IMPLEMENT);
  await phase("implement");
  await run.until(
    "async (a, lq) => lq.all('.inspector-loops .loop-implement dd').some((d) => lq.text(d).includes('contract risk no'))",
    {},
    { what: "the plan's revision and contract risk" },
  );
  await run.shots("implement-implementing", { root: ".inspector" });
  await poll(() => f.runs() >= first, "the first review run starts", 90000);
  await phase("ladder");
  const rounds = await run.until(
    `async (a, lq) => {
      const r = lq.all('.inspector-loops .loop-rounds li').map(lq.text);
      if (!r.some((t) => /under review/.test(t))) return false;
      document.querySelector('.inspector-loops .loop-rounds')?.scrollIntoView({ block: 'center' });
      await lq.settle();
      return { rounds: r, implement: lq.all('.inspector-loops .loop-implement dd').map(lq.text), phases: lq.all('.inspector-loops .loop-phases li').map(lq.text) };
    }`,
    {},
    { what: "the round under review" },
  );
  await run.shots("implement-rounds", { root: ".inspector" });
  run.pass(
    "Start and save runs it: the IMPLEMENT LOOP chip, the inspector's plan, implement and rounds",
    rounds,
  );

  // 5. The end, with its commit (the QA commits for the fake thread at the report turn).
  f.releaseAll();
  await poll(
    async () => (await runningLoop(d, BUILDER))?.phase === "report",
    "the implementation loop reports",
    120000,
  );
  f.commit((await threadView(d, BUILDER)).worktree);
  const last = await poll(async () => {
    const t = await threadView(d, BUILDER);
    return !t.loop && t.last_loop?.kind === "implement" && t.last_loop;
  }, "the implementation loop ends");
  assert.equal(last.outcome, "clean", JSON.stringify(last));
  assert.equal(last.commits?.length, 1, JSON.stringify(last));
  const sha = last.commits[0].sha.slice(0, 7);
  await run.until(
    "async (a, lq) => lq.text(document.querySelector('.inspector-loops .loop-last')).includes('Last implementation loop: clean') && lq.all('.inspector-loops .loop-commits code').some((c) => lq.text(c).includes(a.sha))",
    { sha },
    { what: "the ended loop and its commit in the inspector" },
  );
  await run.shots("implement-ended", { root: ".inspector" });

  // 6. Open report: the report's text as a sheet that scrolls.
  await run.tap(".inspector-loops button", "Open report");
  await run.until(
    "async (a, lq) => !!lq.find('[role=dialog] .loop-report-body .markdown')",
    {},
    { what: "the report's text", timeout: 30000 },
  );
  const reportSheet = await run.ev(`async (a, lq) => {
    const dialog = [...document.querySelectorAll('[role=dialog]')].at(-1);
    const body = dialog.querySelector('.loop-report-body');
    const before = body.scrollTop;
    body.scrollTop = body.scrollHeight;
    await lq.settle();
    const end = body.scrollTop;
    const lastLine = [...body.querySelectorAll('.markdown > *')].at(-1);
    const b = body.getBoundingClientRect(), l = lastLine.getBoundingClientRect();
    body.scrollTop = 0;
    await lq.settle();
    return {
      title: lq.text(dialog.querySelector('h2')),
      text: lq.text(body),
      scrolls: getComputedStyle(body).overflowY,
      scrolled: end > before,
      // The last block's end shows (a block taller than the sheet starts above it).
      lastShown: l.bottom <= b.bottom + 0.5 && l.bottom > b.top,
      boxes: { body: b.toJSON(), last: l.toJSON(), height: body.scrollHeight },
      region: body.getAttribute('role') + ' ' + lq.nameOf(body),
      // A sheet across the screen, not a column of the inspector's panel.
      width: Math.round(dialog.getBoundingClientRect().width),
      viewport: document.documentElement.clientWidth,
      inInspector: !!dialog.closest('.inspector'),
      path: lq.text(dialog.querySelector('.loop-report-footer .loop-path code')),
    };
  }`);
  assert.equal(reportSheet.title, `Implementation loop report · ${BUILDER}`);
  assert.match(reportSheet.text, /Built the CSV export/);
  assert.match(reportSheet.text, new RegExp(sha));
  assert.equal(reportSheet.scrolls, "auto");
  assert(
    reportSheet.scrolled,
    "The report scrolls: " + JSON.stringify(reportSheet),
  );
  assert(
    reportSheet.lastShown,
    "Its last line can be scrolled to: " + JSON.stringify(reportSheet.boxes),
  );
  assert.equal(reportSheet.region, "region Report");
  assert(
    !reportSheet.inInspector && reportSheet.width >= reportSheet.viewport - 2,
    "The report is a sheet across the screen: " + JSON.stringify(reportSheet),
  );
  assert.match(reportSheet.path, /report\.md$/);
  await run.shots("implement-report-sheet", { root: "[role=dialog]" });
  const reachFooter = await run.ev(LAST_REACHABLE, {
    selector: ".loop-report-footer button",
  });
  assert(reachFooter.reachable, JSON.stringify(reachFooter));
  await d.orientation("landscape");
  await delay(1500);
  await run.shots("implement-report-sheet-landscape", {
    root: "[role=dialog]",
    modes: ["dark"],
  });
  assert(
    (await run.ev(LAST_REACHABLE, { selector: ".loop-report-footer button" }))
      .reachable,
    "Sideways the report sheet reaches its footer",
  );
  await d.orientation("portrait");
  await delay(1500);
  await d.textSize("largest");
  await run.until(
    "async () => parseFloat(getComputedStyle(document.documentElement).fontSize) >= 24",
    {},
    { what: "the largest text size", timeout: 20000 },
  );
  await run.shots("implement-report-sheet-largest-text", {
    root: "[role=dialog]",
    modes: ["dark"],
  });
  assert(
    (await run.ev(LAST_REACHABLE, { selector: ".loop-report-footer button" }))
      .reachable,
    "At the largest text the report sheet reaches its footer",
  );
  const largeBody = await run.ev(
    "async (a, lq) => { const b = [...document.querySelectorAll('[role=dialog]')].at(-1).querySelector('.loop-report-body'); return { height: Math.round(b.clientHeight), font: parseFloat(getComputedStyle(b).fontSize) }; }",
  );
  assert(
    largeBody.height >= 2.5 * largeBody.font,
    "At the largest text the report's text keeps room to read: " +
      JSON.stringify(largeBody),
  );
  await d.textSize("default");
  await run.until(
    "async () => parseFloat(getComputedStyle(document.documentElement).fontSize) < 20",
    {},
    { what: "the default text size", timeout: 20000 },
  );
  await run.tap("[role=dialog] button", "Close dialog");
  await run.tab("Threads");
  await run.until(
    "async (a, lq) => { const i = lq.all('.thread-item').find((i) => lq.text(i).includes(a.name)); return lq.text(i?.querySelector('.loop-ended')).includes('committed ' + a.sha); }",
    { name: BUILDER, sha },
    { what: "the card's ended line with the commit" },
  );
  await run.shots("implement-ended-card", { root: ".thread-grid" });
  run.pass(
    "the loop ends with its commit; Open report shows the report's text as a sheet that scrolls",
    {
      last,
      reportSheet: { ...reportSheet, text: reportSheet.text.slice(0, 200) },
    },
  );

  // 7. The New thread form's switch, on the saved settings.
  await run.tab("Chat");
  await run.tap(".conversation-menu > summary");
  await run.tap(".conversation-menu button", "New thread");
  await run.until(
    "async (a, lq) => !!lq.find('[role=dialog] [role=switch]', 'Run as an implementation loop')",
    {},
    { what: "the New thread form's switch" },
  );
  await run.ev(
    `async (a, lq) => {
      const d = [...document.querySelectorAll('[role=dialog]')].at(-1);
      const set = (name, value) => {
        const e = lq.need('input, textarea', { re: '^' + name }, d);
        const proto = e.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
        Object.getOwnPropertyDescriptor(proto, 'value').set.call(e, value);
        e.dispatchEvent(new Event('input', { bubbles: true }));
      };
      set('Project or repository', 'demo');
      set('Name', 'loop-form');
      set('First prompt', a.task);
      document.activeElement?.blur();
      await window.Capacitor?.Plugins?.Keyboard?.hide?.().catch?.(() => {});
      return true;
    }`,
    { task: TASK },
  );
  await run.tap("[role=dialog] [role=switch]", "Run as an implementation loop");
  const form = await run.until(
    `async (a, lq) => {
      const d = [...document.querySelectorAll('[role=dialog]')].at(-1);
      const sw = lq.find('[role=switch]', 'Run as an implementation loop', d);
      const line = lq.text(d.querySelector('.new-thread-loop .loop-settings-line'));
      if (!sw?.checked || !/saved settings/.test(line)) return false;
      sw.scrollIntoView({ block: 'start' });
      await lq.settle();
      return { line, change: !!lq.find('button', 'Change the loop settings…', d) };
    }`,
    {},
    { what: "the switch on, with the saved settings' line" },
  );
  assert(form.change, "Change… beside the settings line");
  await run.shots("new-thread-loop", { root: "[role=dialog]" });
  await run.tap("[role=dialog] button", "Create thread");
  await poll(async () => {
    const t = await threadView(d, "loop-form").catch(() => undefined);
    return t && (t.loop || t.last_loop);
  }, "the New thread form's switch starts an implementation loop");
  const formLoop = (await threadView(d, "loop-form")).loop;
  if (formLoop) {
    assert.equal(formLoop.kind, "implement");
    await owner("loop.control", { thread: "loop-form", action: "stop" });
    await poll(
      async () => !(await threadView(d, "loop-form")).loop,
      "loop-form's loop stops",
    );
  }
  run.pass("the New thread form's switch starts an implementation loop", form);

  // 8. Offline: neither Send as implementation loop nor the card's answers start
  //    anything, and nothing goes to the outbox.
  await owner("loop.request", {
    thread: BUILDER,
    kind: "implement",
    task: TASK,
    setup: true,
  });
  await openConversation(run, BUILDER);
  await run.until(
    "async (a, lq) => !!lq.find('.loop-setup-row button', 'Review setup')",
    {},
    { what: "the raised card's row" },
  );
  await typeTask(run, BUILDER, "add a PDF export");
  await d.offline(true);
  const offlineChoice = await poll(async () => {
    const c = await openSendAsLoop(run, BUILDER);
    if (c.disabled) return c;
    await run.ev(
      "async (a, lq) => { lq.find('.send-loop-button', { re: '^Send as implementation loop to' })?.click(); return true; }",
    );
    return false;
  }, "Send as implementation loop is off while offline");
  const offline = await run.ev(`async (a, lq) => {
    lq.find('.send-loop-panel button', 'Send as implementation loop')?.click();
    await lq.sleep(500);
    lq.find('.send-loop-button', { re: '^Send as implementation loop to' })?.click();
    lq.find('.loop-setup-row button', 'Review setup')?.click();
    await lq.sleep(800);
    const start = lq.find('[role=dialog] .attention-options button', 'Start and save');
    const without = lq.find('[role=dialog] .attention-options button', 'Run without a loop');
    start?.click();
    without?.click();
    await lq.sleep(800);
    const journal = await window.Capacitor?.Plugins?.TwrOffline?.list?.().catch(() => undefined);
    return { start: start ? start.disabled : null, without: without ? without.disabled : null, outbox: lq.text(document.querySelector('.outbox')), journal };
  }`);
  await run.shots("implement-offline", { root: "[role=dialog]" });
  await d.offline(false);
  await run.until(
    "async () => { try { await window.twr.rpc('hub.info'); return true; } catch { return false; } }",
    {},
    { what: "back online", timeout: 60000 },
  );
  await delay(3000);
  assert.equal(offlineChoice.disabled, true, JSON.stringify(offlineChoice));
  assert.match(offlineChoice.what, /live connection/);
  assert.equal(offline.start ?? true, true, JSON.stringify(offline));
  assert.equal(offline.without ?? true, true, JSON.stringify(offline));
  assert(
    !/PDF|CSV|loop/i.test(offline.outbox || ""),
    "Nothing in the outbox: " + offline.outbox,
  );
  assert(
    !JSON.stringify(offline.journal || "").includes("PDF"),
    "Nothing in the offline journal: " + JSON.stringify(offline.journal),
  );
  assert.equal(await runningLoop(d, BUILDER), undefined, "Nothing started");
  run.pass(
    "offline: Send as implementation loop and the card's answers are off; nothing queued or sent",
    { offlineChoice, offline },
  );
  // Leave no card or draft behind.
  const items = await owner("attention.list", {});
  for (const item of items.filter((i) =>
    JSON.stringify(i).includes("loop_setup"),
  ))
    await owner("attention.respond", {
      attn_id: item.attn_id,
      revision: item.revision,
      answer: "cancel",
    });
  await run.ev(
    "async (a, lq) => { lq.find('[role=dialog] button', 'Close dialog')?.click(); await lq.sleep(300); return true; }",
  );
  await typeTask(run, BUILDER, "");
}

/** Threads the surfaces QA makes: the hub gives the first threads the most different
 * colours, so the cards show several. */
const SURFACE_THREADS = [
  "docs-site",
  "api-auth",
  "ui-polish",
  "db-migrate",
  "perf-budget",
];

/**
 * The surfaces every screen shares, audited whole (names, 44-point targets, contrast,
 * clipping, safe areas) as the loop screens are: the thread cards in several colours, a
 * selected card and its details (the name in the thread's colour), the conversation's
 * header, and the whole composer, the keyboard open and a draft in it (the "To:" chip in
 * the thread's colour, the message, Upload, the microphone, hands-free, Send and Send as
 * implementation loop) in portrait, in landscape, at the largest text and, where the
 * driver can narrow the screen (`driver.width`), at 360 and 390 points; each in the light
 * and the dark appearance. Each composer screen's control boxes go in the report.
 */
export async function surfacesQA(driver, fixture, options) {
  const run = new Run(driver, fixture, {
    ...options,
    label: `${options.label || driver.platform}-surfaces`,
  });
  const d = driver;
  for (const name of SURFACE_THREADS)
    await d.owner("thread.new", { project: "demo", name, runtime: "fake" });
  await run.tab("Threads");
  await run.until(
    "async (a, lq) => a.names.every((n) => lq.all('.thread-card').some((c) => lq.text(c).startsWith(n)))",
    { names: SURFACE_THREADS },
    { what: "the new threads' cards" },
  );
  await run.ev(
    "async (a, lq) => { document.querySelector('.thread-grid')?.scrollTo?.(0, 0); document.querySelector('.thread-card')?.scrollIntoView({ block: 'start' }); await lq.settle(); return true; }",
  );
  await run.shots("cards", { root: ".thread-grid" });
  // The thread whose colour is the lightest (the hardest to read in the light theme).
  const thread = await run.ev(
    `async (a, lq) => {
    const lum = (hex) => { const v = parseInt(hex.trim().slice(1), 16); return [16, 8, 0].map((s) => ((v >> s) & 255) / 255).map((c) => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4)).reduce((t, c, i) => t + c * [0.2126, 0.7152, 0.0722][i], 0); };
    const cards = lq.all('.thread-card').map((c) => ({ name: lq.text(c.querySelector('.thread-card-title > b')), color: getComputedStyle(c).getPropertyValue('--thread-color') }));
    return cards.filter((c) => a.names.includes(c.name) && /^\\s*#[0-9a-f]{6}\\s*$/i.test(c.color)).sort((x, y) => lum(y.color) - lum(x.color))[0].name;
  }`,
    { names: SURFACE_THREADS },
  );
  await run.tap(".thread-card", { re: `^${thread}` });
  await run.until("async (a, lq) => !!lq.find('.inspector h2', a.name)", {
    name: thread,
  });
  await run.shots("details", { root: ".inspector" });
  await run.tap("button", "Talk to this thread");
  await run.until(
    "async (a, lq) => !!lq.find('.composer textarea', 'Message ' + a.name + ' directly')",
    { name: thread },
    { what: `the conversation with ${thread}` },
  );
  await run.ev(
    "async () => { document.activeElement?.blur(); await window.Capacitor.Plugins.Keyboard?.hide?.().catch?.(() => {}); return true; }",
  );
  await delay(500);
  await run.shots("conversation", {
    root: ".conversation-heading, .composer",
  });

  /** The composer with a draft and the keyboard open: its screens and its controls' boxes. */
  const composer = async (name, opts = {}) => {
    await run.ev(
      `async (a, lq) => {
        const input = lq.need('.composer textarea');
        input.focus();
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set.call(input, a.draft);
        input.dispatchEvent(new Event('input', { bubbles: true }));
        await window.Capacitor.Plugins.Keyboard?.show?.().catch?.(() => {});
        return true;
      }`,
      { draft: "Check the build on a narrow phone" },
    );
    let keyboard = await run
      .until(
        "async () => document.documentElement.dataset.keyboard === 'true'",
        {},
        { timeout: 8000, what: "the keyboard" },
      )
      .then(() => true)
      .catch(() => false);
    // A Simulator with its hardware keyboard shows none: the composer gets the height a
    // keyboard leaves (336 points in portrait, 209 in landscape), as RemoteHubTests does.
    if (!keyboard)
      keyboard = await run.ev(`async (a, lq) => {
        const root = document.documentElement;
        window.__surfacesHeight = root.style.getPropertyValue('--viewport-height');
        root.style.setProperty('--viewport-height', (innerHeight - (innerWidth > innerHeight ? 209 : 336)) + 'px');
        root.dataset.keyboard = 'true';
        await lq.settle();
        return 'emulated';
      }`);
    await delay(600);
    await run.shots(name, { root: ".composer", ...opts });
    const boxes = await run.ev(`async (a, lq) => {
      const composer = lq.need('.composer');
      const controls = lq.all('button, label.attachment-button, textarea', composer);
      return {
        viewport: { width: innerWidth, height: Math.round(visualViewport?.height || innerHeight) },
        composer: composer.getBoundingClientRect().toJSON(),
        controls: controls.map((e) => { const r = e.getBoundingClientRect(); return { name: lq.nameOf(e), x: Math.round(r.x), y: Math.round(r.y), width: Math.round(r.width * 10) / 10, height: Math.round(r.height * 10) / 10 }; }),
      };
    }`);
    // The message grows with its text, wrapped lines included (Composer.tsx, fitToText),
    // up to its cap, then scrolls inside; it shrinks again when the text goes; and the
    // controls stay above the keyboard all along.
    const fill = (text) =>
      run.ev(
        `async (a, lq) => {
          const input = lq.need('.composer textarea');
          Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set.call(input, a.text);
          input.dispatchEvent(new Event('input', { bubbles: true }));
          await lq.settle();
          const style = getComputedStyle(input);
          const controls = lq.all('button, label.attachment-button', lq.need('.composer-bottom')).map((e) => e.getBoundingClientRect());
          return {
            height: Math.round(input.getBoundingClientRect().height * 10) / 10,
            min: parseFloat(style.minHeight),
            cap: parseFloat(style.maxHeight),
            lineHeight: parseFloat(style.lineHeight),
            hidden: input.scrollHeight - input.clientHeight,
            controls: controls.length,
            controlsBottom: Math.round(Math.max(...controls.map((r) => r.bottom))),
            controlsRight: Math.round(Math.max(...controls.map((r) => r.right))),
            visible: Math.round(Math.min(visualViewport?.height || innerHeight, document.body.getBoundingClientRect().bottom)),
            width: innerWidth,
          };
        }`,
        { text },
      );
    const words =
      "Keep every option labelled and in the same order for the keyboard as on screen. ";
    const growth = { empty: await fill("") };
    growth.two = await fill(words.repeat(2));
    await run.shots(`${name}-grown`, {
      root: ".composer",
      modes: ["dark"],
    });
    growth.long = await fill(words.repeat(14));
    growth.cleared = await fill("");
    await fill("Check the build on a narrow phone");
    const { empty, two, long, cleared } = growth;
    const room = empty.cap - empty.height >= empty.lineHeight / 2;
    const problems = [];
    if (
      room &&
      !(
        two.height > empty.height &&
        (two.hidden <= 1 || Math.abs(two.height - two.cap) <= 1)
      )
    )
      problems.push("a wrapped message did not make the field taller");
    if (!(
      Math.abs(long.height - Math.max(long.cap, long.min)) <= 1 &&
      long.hidden > 0
    ))
      problems.push(
        "a long message did not grow the field to its cap and scroll inside",
      );
    if (Math.abs(cleared.height - empty.height) > 1)
      problems.push("the field did not shrink back when the text went");
    for (const box of [two, long])
      if (
        box.controls < 3 ||
        box.controlsBottom > box.visible + 1 ||
        box.controlsRight > box.width + 1
      )
        problems.push("a control left the screen or went under the keyboard");
    if (problems.length)
      throw new Error(
        `composer ${name}: ${problems.join("; ")}: ${JSON.stringify(growth)}`,
      );
    run.pass(`composer ${name}`, { keyboard, ...boxes, growth });
    (run.report.composer ||= []).push({ name, keyboard, ...boxes, growth });
    await run.ev(
      "async () => { const root = document.documentElement; if (window.__surfacesHeight !== undefined) { root.style.setProperty('--viewport-height', window.__surfacesHeight); root.dataset.keyboard = 'false'; delete window.__surfacesHeight; } document.activeElement?.blur(); await window.Capacitor.Plugins.Keyboard?.hide?.().catch?.(() => {}); return true; }",
    );
    await delay(500);
  };
  try {
    await composer("composer");
    if (d.width) {
      for (const width of [390, 360]) {
        await d.width(width);
        await delay(1500);
        await composer(`composer-${width}`);
      }
      await d.width(null);
      await delay(1500);
    }
    await d.orientation("landscape");
    await delay(1500);
    await composer("composer-landscape");
    await d.orientation("portrait");
    await delay(1500);
    await d.textSize("largest");
    await delay(1500);
    await composer("composer-largest-text");
    if (d.width) {
      await d.width(360);
      await delay(1500);
      await composer("composer-360-largest-text");
    }
  } finally {
    if (d.width) await d.width(null);
    await d.orientation("portrait");
    await d.textSize("default");
    await delay(1500);
  }
  if (run.dir)
    writeFileSync(
      join(run.dir, `${run.label}-qa.json`),
      JSON.stringify(run.report, null, 2),
    );
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
  run.step = options.step || 60;
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
    "async (a, lq) => { const b = lq.find('.inspector-loops .loop-starts button', 'Review loop'); return { disabled: b?.disabled, why: b?.title, report: lq.text(document.querySelector('.inspector-report')) }; }",
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
  await implementReadOnlyQA(run, driver, fixture);
  run.audited();
  return run.report;
}

/**
 * An implementation loop on a `read` device: its card's four answers are off; the chip, the
 * progress and the report's text show; Send as implementation loop and the controls are
 * off.
 */
async function implementReadOnlyQA(run, d, f) {
  const owner = (method, params) => d.owner(method, params);
  // A card holding a task, raised elsewhere.
  await owner("loop.request", {
    thread: BUILDER,
    kind: "implement",
    task: TASK,
    setup: true,
  });
  await run.tab("Needs you");
  await run.until(
    "async (a, lq) => !!lq.all('.loop-setup-row').find((r) => /Implementation loop setup/.test(lq.text(r)))",
    {},
    { what: "the implementation loop's card on a read device" },
  );
  await run.ev(
    "async (a, lq) => { lq.all('.loop-setup-row').find((r) => /Implementation loop setup/.test(lq.text(r))).querySelector('button').click(); await lq.sleep(900); return true; }",
  );
  const card = await run.until(
    "async (a, lq) => { const d = [...document.querySelectorAll('[role=dialog]')].at(-1); const b = lq.all('.attention-options button', d); return b.length && { buttons: b.map((e) => ({ name: lq.nameOf(e), disabled: e.disabled })), task: lq.text(d.querySelector('.loop-setup-task')), why: lq.text(d.querySelector('.loop-readonly')) }; }",
    {},
    { what: "the card's sheet on a read device" },
  );
  assert.deepEqual(
    card.buttons.map((b) => b.name),
    ["Start and save", "Change…", "Run without a loop", "Cancel"],
  );
  assert(
    card.buttons.every((b) => b.disabled),
    "Every answer is off: " + JSON.stringify(card),
  );
  assert.equal(card.task, TASK);
  await run.shots("read-implement-card", { root: "[role=dialog]" });
  await run.tap("[role=dialog] button", "Close dialog");
  for (const item of (await owner("attention.list", {})).filter((i) =>
    JSON.stringify(i).includes("loop_setup"),
  ))
    await owner("attention.respond", {
      attn_id: item.attn_id,
      revision: item.revision,
      answer: "cancel",
    });
  // Started elsewhere on the saved settings: the chip and the progress, nothing to press.
  const before = (await threadView(d, BUILDER)).last_loop?.id;
  const next = f.runs() + 1;
  f.hold(next);
  const started = await owner("loop.request", {
    thread: BUILDER,
    kind: "implement",
    task: "add a JSON export to the reports page",
  });
  assert.equal(started.status, "started", JSON.stringify(started));
  await run.tab("Threads");
  await run.until(
    "async (a, lq) => { const i = lq.all('.thread-item').find((i) => lq.text(i).includes(a.name)); return lq.text(i?.querySelector('.loop-chip .loop-badge')) === 'IMPLEMENT LOOP'; }",
    { name: BUILDER },
    { what: "the IMPLEMENT LOOP chip on a read device", timeout: 60000 },
  );
  await run.shots("read-implement-chip", { root: ".thread-grid" });
  await poll(() => f.runs() >= next, "the loop's first review run", 120000);
  await run.tap(".thread-item .loop-chip", { re: "^IMPLEMENT LOOP" });
  const progress = await run.until(
    "async (a, lq) => { const s = document.querySelector('.inspector-loops'); if (!s || !/IMPLEMENT LOOP/.test(lq.text(s.querySelector('.loop-status')))) return false; return { phases: lq.all('.loop-phases li', s).map(lq.text), implement: lq.all('.loop-implement dd', s).map(lq.text), controls: lq.all('.loop-controls button', s).map((b) => ({ name: lq.nameOf(b), disabled: b.disabled })) }; }",
    {},
    { what: "the implementation loop's progress on a read device" },
  );
  assert(progress.implement.length > 0, JSON.stringify(progress));
  assert(
    progress.controls.length && progress.controls.every((c) => c.disabled),
    JSON.stringify(progress),
  );
  await run.shots("read-implement-progress", { root: ".inspector" });
  // Send as implementation loop is off.
  await openConversation(run, BUILDER);
  const send = await run.ev(
    "async (a, lq) => { const o = lq.find('.send-loop-button', { re: '^Send as implementation loop to' }); if (!o) return { shown: false }; o.click(); await lq.sleep(300); const c = lq.find('.send-loop-panel button', 'Send as implementation loop'); const out = { shown: true, disabled: c ? c.disabled : null, why: c ? lq.text(document.getElementById(c.getAttribute('aria-describedby'))) : '' }; o.click(); return out; }",
  );
  assert(
    !send.shown || send.disabled !== false,
    "A read device cannot send as a loop: " + JSON.stringify(send),
  );
  // Released, it runs to its end (nothing commits it: `not committed`) and the report's
  // text shows.
  f.releaseAll();
  const last = await poll(
    async () => {
      const t = await threadView(d, BUILDER);
      return !t.loop && t.last_loop?.id !== before && t.last_loop;
    },
    "the read device's implementation loop ends",
    180000,
  );
  await run.tab("Threads");
  await run.tap(".thread-card", { re: `^${BUILDER}` });
  await run.until(
    "async (a, lq) => !!lq.find('.inspector-loops button', 'Open report')",
    {},
    { what: "Open report on a read device" },
  );
  await run.tap(".inspector-loops button", "Open report");
  const report = await run.until(
    "async (a, lq) => { const d = [...document.querySelectorAll('[role=dialog]')].at(-1); const t = d && lq.text(d.querySelector('.loop-report-body .markdown')); return t && { title: lq.text(d.querySelector('h2')), text: t.slice(0, 200) }; }",
    {},
    { what: "the report's text on a read device", timeout: 30000 },
  );
  assert.equal(report.title, `Implementation loop report · ${BUILDER}`);
  await run.shots("read-implement-report", { root: "[role=dialog]" });
  await run.tap("[role=dialog] button", "Close dialog");
  run.pass(
    "a read device sees an implementation loop's card, chip, progress and report text, and can answer, start or control nothing",
    { card, progress, send, last, report },
  );
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

// ---------------------------------------------------------------------------------------
// Loops on a remote box and ACP reviewers (docs/design/loops.md, "Milestone 4")
// ---------------------------------------------------------------------------------------

/** A free TCP port on loopback. */
const freePort = () =>
  new Promise((done, fail) => {
    const server = createServer();
    server.once("error", fail);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => done(port));
    });
  });
/** What the live regions say, kept on the page (they clear after a few seconds). */
const LISTEN = `async () => {
  window.__said = [];
  new MutationObserver(() => {
    for (const node of document.querySelectorAll("[data-twr-live]"))
      if (node.textContent && window.__said.at(-1) !== node.textContent) window.__said.push(node.textContent);
  }).observe(document.body, { subtree: true, childList: true, characterData: true });
  return true;
}`;
/** A thread's card in the Threads tab, and its loop chip's line. */
const CARD = `async (a, lq) => {
  const item = lq.all('.thread-item').find((i) => lq.text(i.querySelector('.thread-card')).startsWith(a.name));
  if (!item) return false;
  return { text: lq.text(item), chip: lq.text(item.querySelector('.loop-chip .loop-line')), ended: lq.text(item.querySelector('.loop-ended')) };
}`;
/** The inspector's Loops section, as a person reads it. */
const LOOPS = `async (a, lq) => {
  const s = document.querySelector('.inspector-loops');
  if (!s || !lq.visible(s)) return false;
  const pause = lq.find('.loop-controls button', { re: '^(Pause|Resume)$' }, s);
  return {
    status: lq.text(s.querySelector('.loop-status')),
    rounds: lq.all('.loop-rounds > li', s).map(lq.text),
    facts: lq.text(s.querySelector('.loop-facts')),
    why: lq.text(s.querySelector('.loop-start-why')),
    starts: lq.all('.loop-starts button', s).map((b) => ({ name: lq.nameOf(b), disabled: b.disabled })),
    last: lq.text(s.querySelector('.loop-last')),
    pauseTop: pause ? pause.getBoundingClientRect().top - s.getBoundingClientRect().top : null,
  };
}`;
/** The reviewer and implementer pickers and the reviewer's notes of the editor in `root`
 * (the top dialog, or Settings › Loops). */
const PICKERS = `async (a, lq) => {
  const root = a.root === 'dialog' ? [...document.querySelectorAll('[role=dialog]')].at(-1) : lq.all('.settings-section').find((s) => lq.text(s.querySelector('h2')) === 'Loops');
  const group = [...root.querySelectorAll('.loop-editor-group')].find((g) => lq.text(g.querySelector('.loop-editor-heading')) === 'Reviewer');
  if (!group) return false;
  const [runtime, model] = group.querySelectorAll('select');
  const impl = [...root.querySelectorAll('label.field')].find((l) => lq.text(l.querySelector('span')) === 'Implementer')?.querySelector('select');
  const options = (s) => [...(s?.options || [])].map((o) => ({ value: o.value, text: o.textContent, disabled: o.disabled }));
  if (a.pick && runtime.value !== a.pick) {
    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value').set.call(runtime, a.pick);
    runtime.dispatchEvent(new Event('change', { bubbles: true }));
    await lq.settle();
    await lq.sleep(300);
  }
  const described = (runtime.getAttribute('aria-describedby') || '').split(' ').map((id) => lq.text(document.getElementById(id))).join(' ');
  return {
    reviewer: options(runtime),
    value: runtime.value,
    models: options(group.querySelectorAll('select')[1]).map((o) => o.value),
    model: group.querySelectorAll('select')[1]?.value,
    implementer: options(impl).map((o) => o.value),
    skipped: lq.text(root.querySelector('.loop-editor-skipped')),
    acp: lq.text(root.querySelector('.loop-editor-acp')),
    box: lq.text(root.querySelector('.loop-editor-box')),
    described,
  };
}`;

/**
 * Loops on a thread on a remote box, and an ACP preset as the reviewer, on an `operate`
 * phone (docs/design/loops.md, "Milestone 4 implementation notes"). The hub restarts with a
 * runner listener, box `lab`'s token and three ACP presets on the scripted ACP reviewer of
 * the hub's tests; box `lab` is a dial-in `twr runner` on this machine (its own home) with
 * its own fake Codex and Claude Code, which its agent probe finds. Then, each audited in
 * light and dark: the setup card naming the box when its reviewer is not installed there;
 * a reviewer account that is not the box's own login coming back on the card; a review loop
 * whose runner is stopped during its run, waiting on its box in words and said once, after
 * a reload and the app's own reconnect too, and going on from the same round once the
 * runner is back; an implementation loop on the box; nothing offering a start while the box
 * is away; the reviewer pickers listing the ACP presets (the one that reviews with its mode,
 * models and efforts, the others disabled with why), the card's ACP warning, a loop on the
 * ACP reviewer, and the preset removed from the config.
 *
 * The driver adds: `home` (the hub's TWR_HOME), `bin`, `env` (the hub's environment) and
 * `restartHub(extraConfig)`.
 */
export async function remoteLoopQA(driver, fixture, options) {
  const run = new Run(driver, fixture, options);
  run.step = options.step || 80;
  const d = driver;
  const f = fixture;
  const owner = (method, params) => d.owner(method, params);
  const twr = (...args) =>
    execFileSync(join(d.bin, "twr"), args, {
      env: d.env,
      encoding: "utf8",
      stdio: "pipe",
    });
  const home = d.home;
  const port = await freePort();
  const token = twr("runner", "token").trim();
  const boxHome = join(home, "box");
  mkdirSync(boxHome, { recursive: true });
  writeFileSync(join(boxHome, "lab.token"), token + "\n", { mode: 0o600 });
  const revs = join(home, "box-reviewers");
  mkdirSync(revs, { recursive: true });
  const codex = join(revs, "codex");
  writeFileSync(
    codex,
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
  chmodSync(codex, 0o755);
  const claude = join(revs, "claude");
  writeFileSync(
    claude,
    `#!/bin/sh
case "$1" in --version) echo "2.0.0 (Claude Code)"; exit 0;; auth) echo '{"loggedIn": true, "authMethod": "claude.ai"}'; exit 0;; esac
exit 1
`,
  );
  chmodSync(claude, 0o755);
  const boxRuns = () =>
    existsSync(join(revs, "count"))
      ? Number(readFileSync(join(revs, "count"), "utf8").trim()) || 0
      : 0;
  const boxHold = (n) => writeFileSync(join(revs, `hold.${n}`), "");
  const boxRelease = (n) => rmSync(join(revs, `hold.${n}`), { force: true });
  const acpState = join(home, "acp");
  mkdirSync(acpState, { recursive: true });
  const acpHold = (n) => writeFileSync(join(acpState, `hold.${n}`), "");
  const acpRelease = (n) =>
    rmSync(join(acpState, `hold.${n}`), { force: true });
  const agent = `python3 ${resolve("../crates/twr-hub/tests/fixtures/acp/reviewer_agent.py")} ${acpState}`;
  const work = join(home, "accounts", "codex-work");
  mkdirSync(work, { recursive: true, mode: 0o700 });
  const extra = (withEx) =>
    `[hub]\nlisten = "127.0.0.1:${port}"\n[boxes.lab]\ntoken = "${token}"\n[accounts.codex.work]\ndir = ${JSON.stringify(work)}\n` +
    (withEx
      ? `[runtimes.ex]\nacp = ${JSON.stringify(agent)}\n[runtimes.ex.review]\nmode = "read-only"\nmodels = ["m-large", "m-small"]\nefforts = ["medium", "xhigh"]\n`
      : "") +
    `[runtimes.bare]\nacp = ${JSON.stringify(agent)}\n[runtimes.loose]\nacp = ${JSON.stringify(agent)}\n[runtimes.loose.review]\nmode = "default"\n`;
  /** After a hub restart, the app's connection is back. */
  const reconnected = () =>
    run.until(
      "async () => { try { return !!(await window.twr.rpc('hub.info')).pid; } catch { return false; } }",
      {},
      { what: "the app reconnected", timeout: 90000 },
    );
  let runner;
  const startRunner = async (withCodex = true) => {
    const env = {
      ...d.env,
      TWR_HOME: boxHome,
      TWR_TEST_USER_TOKEN_FILE: join(boxHome, "user.token"),
      TWR_BOX_PROBE: "on",
      TWR_CODEX_BIN: withCodex ? codex : join(revs, "no-codex"),
      TWR_CLAUDE_BIN: claude,
      TWR_GROK_BIN: join(revs, "no-grok"),
      XDG_STATE_HOME: join(boxHome, "state"),
    };
    delete env.TWR_TEST_AGENTS;
    runner = spawn(
      join(d.bin, "twr"),
      [
        "runner",
        "--hub",
        `ws://127.0.0.1:${port}`,
        "--box=lab",
        `--token-file=${join(boxHome, "lab.token")}`,
        `--data-dir=${join(boxHome, "data")}`,
      ],
      { env, stdio: "ignore" },
    );
    await poll(() => {
      try {
        return JSON.parse(twr("box", "status", "lab", "--json")).state ===
          "online"
          ? true
          : false;
      } catch {
        return false;
      }
    }, "box lab online");
  };
  const stopRunner = async () => {
    if (!runner || runner.exitCode !== null) return;
    const exited = new Promise((r) => runner.once("exit", r));
    runner.kill("SIGTERM");
    await exited;
  };
  /** The hub has the box's agents as its runner found them (Codex there or not). */
  const boxAgents = (found) =>
    poll(async () => {
      const got = await owner("loop.settings.get", { thread: "boxer" });
      const c = got.runtimes.find((r) => r.runtime === "codex");
      return got.box === "lab" && c?.probed && c.found === found;
    }, "box lab's agents");
  const git = (dir, ...args) =>
    execFileSync(
      "git",
      [
        "-C",
        dir,
        "-c",
        "user.name=QA",
        "-c",
        "user.email=qa@example.invalid",
        ...args,
      ],
      { stdio: "ignore" },
    );
  /** The keyboard put away (a composer just used takes the focus back, and while the
   * keyboard is up the tab bar is hidden). */
  const keyboardAway = () =>
    run.ev(
      "async (a, lq) => { document.activeElement?.blur(); await window.Capacitor?.Plugins?.Keyboard?.hide?.().catch?.(() => {}); await lq.sleep(400); return true; }",
    );
  /** The thread's inspector, from its card on the Threads tab. */
  const inspect = async (name) => {
    await keyboardAway();
    await run.tab("Threads");
    await run.until(
      "async (a, lq) => !!lq.find('.thread-card', { re: '^' + a.name })",
      { name },
      { what: `${name}'s card` },
    );
    await run.tap(".thread-card", { re: `^${name}` });
    await run.until(LOOPS, {}, { what: `${name}'s Loops section` });
  };
  /** The setup card raised in the inspector, opened as its sheet, armed: its text. */
  const setupSheet = async () => {
    await run.until(
      "async (a, lq) => !!lq.find('.inspector .loop-setup-row button', 'Review setup')",
      {},
      { what: "the setup card's row" },
    );
    await run.tap(".inspector .loop-setup-row button", "Review setup");
    await run.until(
      "async (a, lq) => !!lq.find('[role=dialog] .attention-loop-setup') && lq.find('[role=dialog] .attention-options')?.dataset.armed === 'true'",
      {},
      { what: "the setup card's sheet, armed" },
    );
    return run.ev(
      "async (a, lq) => lq.text(document.querySelector('[role=dialog] .attention-loop-setup'))",
    );
  };
  const said = () => run.ev("async () => (window.__said || []).join(' | ')");
  // The settings the loop QA saved, given back at the end: the steps after this one start
  // loops on them.
  const before = (await owner("loop.settings.get", {})).settings;
  try {
    await d.restartHub(extra(true));
    await reconnected();
    await run.ev(LISTEN);
    await startRunner(false);
    const repo = join(home, "box-repo");
    mkdirSync(repo, { recursive: true });
    execFileSync("git", ["init", "-q", "-b", "main", repo]);
    writeFileSync(join(repo, "lib.rs"), "pub fn f() {}\n");
    git(repo, "add", "-A");
    git(repo, "commit", "-q", "-m", "Initial");
    twr(
      "thread",
      "new",
      repo,
      "--box",
      "lab",
      "--runtime",
      "fake",
      "--name",
      "boxer",
      "--mode",
      "auto",
    );
    const boxer = () => threadView(d, "boxer");
    const worktree = await poll(
      async () => (await boxer()).worktree,
      "boxer's worktree",
    );
    writeFileSync(join(worktree, "lib.rs"), "fn parse(s: &str) {}\n");
    await boxAgents(false);
    await owner("loop.settings.reset", {});

    // 1. The box's runner found no Codex: the card reviews with Claude Code, naming the box.
    await inspect("boxer");
    await run.tap(".inspector-loops .loop-starts button", "Review loop");
    const notInstalled = await setupSheet();
    assert(
      notInstalled.includes(
        "codex isn't installed on box lab: reviewing with claude-code · claude-opus-5-5 instead",
      ),
      notInstalled,
    );
    await run.shots("box-not-installed", { root: "[role=dialog]" });
    await run.tap("[role=dialog] .attention-options button", "Change…");
    await run.until(
      "async (a, lq) => !!lq.find('[role=dialog] .loop-presets')",
    );
    const boxEditor = await run.until(PICKERS, { root: "dialog" });
    assert.equal(
      boxEditor.box,
      "Runtimes as found on box lab: its reviewer runs there.",
    );
    assert.equal(
      boxEditor.skipped,
      `Can't review on box lab: codex (not installed); grok-build (not installed); bare (acp preset bare names no read-only mode); loose (acp preset loose's review mode "default" is not read-only: twr knows it as a mode that lets the agent act)`,
    );
    assert.match(boxEditor.described, /Can't review on box lab/);
    await run.shots("box-editor", { root: "[role=dialog]" });
    await run.tap("[role=dialog] button", "Cancel");
    await delay(700);
    await run.tap("[role=dialog] .attention-options button", "Cancel");
    await run.until(
      "async (a, lq) => !lq.find('.attention-loop-setup')",
      {},
      { what: "the card goes on Cancel" },
    );
    run.pass(
      "a box thread's card names the box its reviewer is not installed on; the editor says what can't review there",
    );

    // 2. Saved settings on Codex's account `work`: on the box, which runs every runtime on
    //    its own login, they do not hold; the card comes back on the default account.
    await stopRunner();
    await startRunner(true);
    await boxAgents(true);
    await owner("loop.settings.set", {
      settings: {
        profile: "low",
        implementer: null,
        workers: { model: "claude-opus-5-5", effort: "high" },
        reviewer: { runtime: "codex", account: "work", model: "gpt-6.1-sol" },
      },
    });
    boxHold(1);
    await inspect("boxer");
    await run.tap(".inspector-loops .loop-starts button", "Review loop");
    const account = await setupSheet();
    assert(
      account.includes(
        "box lab runs codex on its own login, not account work: using its default account",
      ),
      account,
    );
    assert.match(account, /a saved setting no longer holds/);
    assert.match(account, /Reviewer codex · gpt-6\.1-sol · medium/);
    await run.shots("box-account", { root: "[role=dialog]" });
    run.pass(
      "a reviewer account other than the box's own login comes back on the card, on the default account, saying why",
    );

    // 3. Start and save; the review run on the box is held; the runner stops.
    await run.tap("[role=dialog] .attention-options button", "Start and save");
    await poll(() => boxRuns() >= 1, "the box's review run starts");
    const chipOf =
      "async (a, lq) => { const c = await (" +
      CARD +
      ")(a, lq); return c && c.chip === a.chip && c; }";
    await run.tab("Threads");
    await run.until(
      chipOf,
      { name: "boxer", chip: "low · ladder m1" },
      { what: "boxer's chip" },
    );
    await inspect("boxer");
    const running = await run.until(
      "async (a, lq) => { const l = await (" +
        LOOPS +
        ")(a, lq); return l && /waiting on the review$/.test(l.status) && l; }",
      {},
      { what: "the loop waits on its review" },
    );
    await stopRunner();
    await poll(
      async () => (await boxer()).loop?.box_away === "lab",
      "the loop waits on its box",
    );
    const away = await run.until(
      "async (a, lq) => { const l = await (" +
        LOOPS +
        ")(a, lq); return l && /^REVIEW LOOP low · waiting on box lab$/.test(l.status) && l.rounds[0] === 'm1 waits for box lab' && l; }",
      {},
      { what: "the inspector says the loop waits on box lab" },
    );
    // (Within a pixel: a position on a fractional pixel may round either way.)
    assert(
      Math.abs(away.pauseTop - running.pauseTop) <= 1,
      `The Loops section's controls stay where they were: ${running.pauseTop} then ${away.pauseTop}`,
    );
    await run.until(
      'async () => (window.__said || []).some((s) => s.includes("boxer\'s review loop waits on box lab: it goes on when the box is back"))',
      {},
      { what: "the wait announced" },
    );
    await run.shots("box-away", { root: ".inspector" });
    await d.textSize("largest");
    await run.until(
      "async () => parseFloat(getComputedStyle(document.documentElement).fontSize) >= 24",
      {},
      { what: "the largest text size", timeout: 20000 },
    );
    await run.ev(
      "async (a, lq) => { document.querySelector('.inspector-loops .loop-status')?.scrollIntoView({ block: 'start' }); await lq.settle(); return true; }",
    );
    await run.shots("box-away-largest-text", {
      root: ".inspector",
      modes: ["dark"],
    });
    await d.textSize("default");
    await run.until(
      "async () => parseFloat(getComputedStyle(document.documentElement).fontSize) < 20",
      {},
      { what: "the default text size", timeout: 20000 },
    );
    // Its controls work while the box is away.
    await run.tap(".inspector-loops .loop-controls button", "Pause");
    await run.until(
      "async (a, lq) => lq.text(document.querySelector('.inspector-loops .loop-status')).endsWith('paused, waiting on box lab')",
      {},
      { what: "paused while away" },
    );
    await run.tap(".inspector-loops .loop-controls button", "Resume");
    await run.until(
      "async (a, lq) => lq.text(document.querySelector('.inspector-loops .loop-status')).endsWith('· waiting on box lab')",
      {},
      { what: "resumed while away" },
    );
    const once = (await said())
      .split(" | ")
      .filter((s) => s.includes("waits on box lab")).length;
    assert.equal(once, 1, "The wait is said once: " + (await said()));
    await run.tab("Threads");
    const chipAway = await run.until(
      chipOf,
      { name: "boxer", chip: "low · ladder m1 · waiting on box lab" },
      { what: "boxer's chip says it waits on box lab" },
    );
    await run.shots("box-away-card", { root: ".thread-grid" });
    run.pass(
      "the runner stopped during a review run: the inspector and the chip say the loop waits on box lab, said once; its controls work",
      { status: away.status, rounds: away.rounds, chip: chipAway?.chip },
    );

    // 4. The app's own connection drops and comes back, then it is opened again (a reload):
    //    the wait shows from the hub's snapshot, and is not said again.
    await d.offline(true);
    await delay(3000);
    await d.offline(false);
    await reconnected();
    await inspect("boxer");
    await run.until(
      "async (a, lq) => lq.text(document.querySelector('.inspector-loops .loop-status')).endsWith('waiting on box lab')",
      {},
      { what: "the wait after a reconnect" },
    );
    await run.ev(
      "async () => { setTimeout(() => location.reload(), 100); return true; }",
    );
    await delay(3000);
    await reconnected();
    await run.ev(LISTEN);
    await inspect("boxer");
    const reopened = await run.until(
      "async (a, lq) => { const l = await (" +
        LOOPS +
        ")(a, lq); return l && l.status.endsWith('waiting on box lab') && l; }",
      {},
      { what: "the wait after a reload", timeout: 60000 },
    );
    await delay(1500);
    assert(
      !(await said()).includes("waits on box lab"),
      "A wait the app opened on is shown, not said",
    );
    await run.shots("box-away-reopened", { root: ".inspector" });
    run.pass(
      "reconnected, and opened again, while the loop waits: the wait shows from the snapshot",
      reopened,
    );

    // 5. The runner back: the loop goes on from round m1, without a reload.
    boxRelease(1);
    boxHold(2);
    await startRunner(true);
    await poll(() => boxRuns() >= 2, "the round's retry starts");
    await run.until(
      'async () => (window.__said || []).some((s) => s.includes("boxer\'s review loop goes on: box lab is back"))',
      {},
      { what: "the box back, announced" },
    );
    const back = await run.until(
      "async (a, lq) => { const l = await (" +
        LOOPS +
        ")(a, lq); return l && /waiting on the review$/.test(l.status) && l.rounds.length === 1 && l.rounds[0] === 'm1 under review again' && l; }",
      {},
      { what: "the loop goes on from m1" },
    );
    await run.shots("box-back", { root: ".inspector" });
    boxRelease(2);
    await poll(async () => {
      const t = await boxer();
      return !t.loop && t.last_loop?.outcome === "clean";
    }, "the box's review loop ends clean");
    await run.until(
      "async (a, lq) => !!lq.find('.inspector-loops button', 'Open report')",
      {},
      { what: "Open report", timeout: 30000 },
    );
    await run.tap(".inspector-loops button", "Open report");
    const report = await run.until(
      "async (a, lq) => { const t = lq.text(document.querySelector('[role=dialog] .loop-report-body')); return t.includes('failed run: box lab disconnected') && t; }",
      {},
      { what: "the report says the run that failed", timeout: 30000 },
    );
    await run.shots("box-report", { root: "[role=dialog]" });
    await run.tap("button[aria-label='Close dialog']");
    run.pass(
      "the runner back, the loop goes on from round m1 without a reload, ends clean, and its report says the run that failed",
      { back: back.rounds, report: report.slice(0, 200) },
    );

    // 6. An implementation loop on the box thread, sent from its composer (the ↻ button).
    f.template(
      "impl-report",
      '!sleep 8000\nBuilt the CSV export.\n{{commit}}\n```twr-commit\n{"commits": [{"repo": "box-repo", "sha": "abc1234"}]}\n```\n',
    );
    await openConversation(run, "boxer");
    await typeTask(run, "boxer", TASK);
    const choice = await openSendAsLoop(run, "boxer");
    assert.equal(choice.disabled, false, JSON.stringify(choice));
    await run.tap(".send-loop-panel button", "Send as implementation loop");
    await poll(
      async () => (await runningLoop(d, "boxer"))?.kind === "implement",
      "the box's implementation loop starts",
    );
    await poll(
      async () => (await runningLoop(d, "boxer"))?.phase === "report",
      "the box's implementation loop reports",
      120000,
    );
    git(worktree, "add", "feature.rs");
    git(worktree, "commit", "-q", "-m", "Add the CSV export");
    const ended = await poll(async () => {
      const t = await boxer();
      return !t.loop && t.last_loop?.kind === "implement" && t.last_loop;
    }, "the box's implementation loop ends");
    assert.equal(ended.outcome, "clean", JSON.stringify(ended));
    assert.equal(ended.commits?.length, 1, JSON.stringify(ended));
    const sha = ended.commits[0].sha.slice(0, 7);
    await keyboardAway();
    await run.tab("Threads");
    await run.until(
      "async (a, lq) => { const c = await (" +
        CARD +
        ")(a, lq); return c && c.ended.includes('committed ' + a.sha); }",
      { name: "boxer", sha },
      { what: "boxer's card says what the loop committed" },
    );
    await run.shots("box-implement-ended", { root: ".thread-grid" });
    f.template(
      "impl-report",
      '!sleep 8000\nBuilt the CSV export.\n{{commit}}\n```twr-commit\n{"commits": [{"repo": "demo", "sha": "abc1234"}]}\n```\n',
    );
    run.pass(
      "an implementation loop on the box thread, sent with the ↻ button, reviews on the box and commits",
      { sha },
    );

    // 7. The box away with no loop running: nothing offers a start the hub would refuse.
    await stopRunner();
    await poll(
      async () => (await boxer()).box_online === false,
      "box lab offline",
    );
    await inspect("boxer");
    const gated = await run.until(
      "async (a, lq) => { const l = await (" +
        LOOPS +
        ")(a, lq); return l && l.why === 'Box lab is away: a loop can start once it is back.' && l; }",
      {},
      { what: "why no loop can start" },
    );
    assert(
      gated.starts.every((s) => s.disabled),
      JSON.stringify(gated.starts),
    );
    await run.shots("box-offline-start", { root: ".inspector" });
    await keyboardAway();
    await openConversation(run, "boxer");
    const away2 = await openSendAsLoop(run, "boxer");
    assert.equal(away2.disabled, true);
    assert.equal(
      away2.what,
      "Box lab is away: a loop can start once it is back.",
    );
    const escape = await run.ev(`async (a, lq) => {
      const panel = document.querySelector('.send-loop-panel');
      const focused = document.activeElement === panel;
      document.activeElement.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }));
      await lq.sleep(300);
      return { focused, closed: !document.querySelector('.send-loop-panel'), back: lq.nameOf(document.activeElement) };
    }`);
    assert.deepEqual(escape, {
      focused: true,
      closed: true,
      back: "Send as implementation loop to boxer…",
    });
    await typeTask(run, "boxer", "");
    run.pass(
      "with the box away, Review loop, Implementation loop… and ↻ say why they wait; the panel takes the focus and Escape closes it",
      { why: gated.why, escape },
    );
    // The box back for the rest.
    await startRunner(true);

    // 8. An ACP preset as the reviewer.
    await owner("loop.settings.reset", {});
    await owner("thread.new", {
      project: "demo",
      name: "acpw",
      runtime: "fake",
      mode: "auto",
    });
    await owner("thread.send", {
      thread: "acpw",
      prompt: "!write lib.rs pub fn g() { todo!() }",
    });
    await poll(async () => {
      const t = await threadView(d, "acpw");
      return (
        t.current_turn == null &&
        Number(t.last_turn?.turn) > 0 &&
        Number(t.git?.dirty_files) > 0
      );
    }, "acpw's turn ends with changes");
    await inspect("acpw");
    await run.tap(".inspector-loops .loop-starts button", "Review loop");
    await setupSheet();
    await run.tap("[role=dialog] .attention-options button", "Change…");
    await run.until(
      "async (a, lq) => !!lq.find('[role=dialog] .loop-presets')",
    );
    const checkPickers = (p, where) => {
      const named = (v) => p.reviewer.find((o) => o.value === v);
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
      assert.equal(named("loose")?.disabled, true);
      assert.deepEqual(
        p.implementer.filter((v) => ["ex", "bare", "loose"].includes(v)),
        [],
        `${where}: no ACP preset as the implementer`,
      );
      assert.equal(
        p.skipped,
        `Can't review: grok-build (not installed); bare (acp preset bare names no read-only mode); loose (acp preset loose's review mode "default" is not read-only: twr knows it as a mode that lets the agent act)`,
      );
      assert.match(p.described, /Can't review: grok-build/);
    };
    checkPickers(await run.until(PICKERS, { root: "dialog" }), "the card");
    const picked = await run.ev(PICKERS, { root: "dialog", pick: "ex" });
    assert.equal(picked.value, "ex");
    assert.equal(picked.model, "m-large");
    assert.deepEqual(
      picked.models.filter((m) => m.startsWith("m-")),
      ["m-large", "m-small"],
    );
    assert.equal(
      picked.acp,
      'ex is an ACP agent: it reviews in its own "read-only" mode, at the efforts medium, xhigh, which every round of the profile must use.',
    );
    await run.shots("acp-editor", { root: "[role=dialog]" });
    await d.textSize("largest");
    await run.until(
      "async () => parseFloat(getComputedStyle(document.documentElement).fontSize) >= 24",
      {},
      { what: "the largest text size", timeout: 20000 },
    );
    await run.ev(
      "async (a, lq) => { [...document.querySelectorAll('[role=dialog]')].at(-1).querySelector('.loop-editor-skipped')?.scrollIntoView({ block: 'center' }); await lq.settle(); return true; }",
    );
    await run.shots("acp-editor-largest-text", {
      root: "[role=dialog]",
      modes: ["dark"],
    });
    await d.textSize("default");
    await run.until(
      "async () => parseFloat(getComputedStyle(document.documentElement).fontSize) < 20",
      {},
      { what: "the default text size", timeout: 20000 },
    );
    await run.tap("[role=dialog] button", "Save to the card");
    // The card is raised again at a new revision: one sheet, the one it was opened from,
    // and it can be used (two sheets would each make the other inert).
    const sheets = await run.until(
      "async (a, lq) => { const d = [...document.querySelectorAll('[role=dialog]')]; return d.length === 1 && !!d[0].querySelector('.attention-loop-setup') && !d[0].closest('[inert]') && d.length; }",
      {},
      { what: "one setup sheet after the editor saved" },
    );
    assert.equal(sheets, 1);
    const acpCard = await run.until(
      "async (a, lq) => { if (lq.find('[role=dialog] .loop-presets')) return false; return lq.all('[role=dialog] .attention-loop-setup').map(lq.text).find((t) => t.includes('Reviewer ex · m-large')) || false; }",
      {},
      { what: "the card on the ACP reviewer" },
    );
    assert(
      acpCard.includes(
        `acp reviewer: read-only rests on the agent's own "read-only" mode`,
      ),
      acpCard,
    );
    await run.shots("acp-card", { root: "[role=dialog]" });
    run.pass(
      "the card's editor lists the ACP presets (the one that reviews with its mode, models and efforts; the others disabled, with why, in words); none as implementer; the card warns what read-only rests on",
    );
    acpHold(1);
    await delay(700);
    await run.tap("[role=dialog] .attention-options button", "Start and save");
    await poll(
      async () => (await runningLoop(d, "acpw"))?.waiting_on === "review",
      "the ACP review run starts",
    );
    await keyboardAway();
    await run.tab("Threads");
    await run.until(
      "async (a, lq) => { const c = await (" +
        CARD +
        ")(a, lq); return c && c.text.includes('reviewer ex m-large medium'); }",
      { name: "acpw" },
      { what: "acpw's card names the ACP reviewer" },
    );
    await run.shots("acp-running", { root: ".thread-grid" });
    acpRelease(1);
    await poll(
      async () => {
        const t = await threadView(d, "acpw");
        return !t.loop && t.last_loop?.outcome === "clean";
      },
      "the ACP review loop ends clean",
      120000,
    );
    await inspect("acpw");
    await run.until(
      "async (a, lq) => !!lq.find('.inspector-loops button', 'Open report')",
      {},
      { what: "Open report", timeout: 30000 },
    );
    await run.tap(".inspector-loops button", "Open report");
    await run.until(
      "async (a, lq) => lq.text(document.querySelector('[role=dialog] .loop-report-body')).includes('Reviewer: ex m-large')",
      {},
      { what: "the report names the ACP reviewer", timeout: 30000 },
    );
    await run.tap("button[aria-label='Close dialog']");
    run.pass(
      "a review loop on the ACP reviewer runs to its end, the reviewer named on the card and in the report",
    );
    // Settings › Loops and a project's editor.
    await keyboardAway();
    await openSettingsLoops(run);
    const panel = await run.until(PICKERS, { root: "settings" });
    checkPickers(panel, "Settings › Loops");
    assert.equal(panel.value, "ex", "The saved reviewer");
    await run.shots("acp-settings", {
      root: ".settings-section:has(.loop-editor)",
    });
    await openConversation(run, "acpw");
    await typeTask(run, "acpw", "/loop settings --project demo");
    await run.tap("button", "Send message");
    await run.until(
      "async (a, lq) => { const d = [...document.querySelectorAll('[role=dialog]')].at(-1); return !!d && lq.text(d.querySelector('h2')) === 'Loop settings · demo' && !!d.querySelector('.loop-presets'); }",
      {},
      { what: "demo's settings editor" },
    );
    checkPickers(await run.until(PICKERS, { root: "dialog" }), "demo's editor");
    await run.shots("acp-project", { root: "[role=dialog]" });
    await run.tap("[role=dialog] button", { re: "^(Cancel|Close)$" });
    run.pass(
      "Settings › Loops and a project's editor list the ACP presets the same way",
    );
    // The preset removed from the config while it is the saved reviewer.
    await d.restartHub(extra(false));
    await reconnected();
    await inspect("acpw");
    // The app's own connection is live again: Review loop can be pressed.
    await run.until(
      "async (a, lq) => { const b = lq.find('.inspector-loops .loop-starts button', 'Review loop'); return !!b && !b.disabled; }",
      {},
      { what: "Review loop enabled after the hub restarted", timeout: 90000 },
    );
    await run.tap(".inspector-loops .loop-starts button", "Review loop");
    const removed = await setupSheet().catch(async (error) => {
      const items = await owner("attention.list", { all: true }).catch((e) =>
        String(e),
      );
      console.log("LOOP_QA_ATTENTION", JSON.stringify(items).slice(0, 3000));
      throw error;
    });
    assert(
      removed.includes(
        "ex is no reviewer: neither a built-in one (claude-code, codex, grok-build) nor an acp preset in the config: reviewing with claude-code · claude-opus-5-5 instead",
      ),
      removed,
    );
    assert.match(removed, /Reviewer claude-code · claude-opus-5-5/);
    await run.shots("acp-removed", { root: "[role=dialog]" });
    await run.tap("[role=dialog] .attention-options button", "Cancel");
    await run.until(
      "async (a, lq) => !lq.find('.attention-loop-setup')",
      {},
      { what: "the card goes on Cancel" },
    );
    run.pass(
      "the saved ACP reviewer removed from the config: the next card says so and offers the replacement",
    );
    const errors = await run.ev("async () => window.__loopErrors || []");
    assert.deepEqual(errors, [], "No page errors");
    await keyboardAway();
    await run.tab("Chat");
    run.audited();
    return run.report;
  } finally {
    await (
      before
        ? owner("loop.settings.set", { settings: before })
        : owner("loop.settings.reset", {})
    ).catch(() => {});
    boxRelease(1);
    boxRelease(2);
    acpRelease(1);
    await stopRunner().catch(() => {});
    try {
      execFileSync(join(d.bin, "twr"), ["runner", "stop"], {
        env: { ...d.env, TWR_HOME: boxHome },
        stdio: "ignore",
        timeout: 15000,
      });
    } catch {}
  }
}
