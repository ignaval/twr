import { expect } from "@playwright/test";
import { qaSource } from "./qa-source.mjs";
import { mkdirSync, writeFileSync } from "node:fs";
export async function performanceAudit(page, label, application) {
  if (
    await page.evaluate(
      () => document.documentElement.dataset.mobile === "true",
    )
  ) {
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
      .poll(() =>
        page.evaluate(
          () => document.documentElement.dataset.keyboard !== "true",
        ),
      )
      .toBe(true);
  }
  const count = 1000;
  const rpc = (method, params = {}) =>
    page.evaluate(({ method, params }) => window.twr.rpc(method, params), {
      method,
      params,
    });
  await rpc("thread.new", {
    project: "demo",
    name: "performance-audit",
    runtime: "fake",
  });
  const tabs = page
    .locator(".mobile-tabs, .compact-nav")
    .getByRole("button", { name: "Threads", exact: true });
  if (await tabs.isVisible()) await tabs.click();
  else {
    const show = page.getByRole("button", {
      name: "Show sidebar",
      exact: true,
    });
    if (await show.isVisible()) await show.click();
    await page
      .locator(".primary-nav")
      .getByRole("button", { name: /^Thread grid/ })
      .click();
  }
  await page
    .locator(".thread-grid .thread-card")
    .filter({ hasText: "performance-audit" })
    .click();
  await page
    .getByRole("button", { name: "Talk to this thread", exact: true })
    .click();
  await page
    .getByRole("textbox", {
      name: "Message performance-audit directly",
      exact: true,
    })
    .waitFor();
  await page.evaluate(() => {
    const sample = {
      frames: [],
      longTasks: [],
      inputPaint: [],
      inputFrames: [],
      focusChanges: [],
      startedAt: performance.now(),
      running: true,
      last: 0,
    };
    window.__twrPerf = sample;
    const frame = (now) => {
      if (sample.last) sample.frames.push(now - sample.last);
      sample.last = now;
      if (sample.running) requestAnimationFrame(frame);
    };
    requestAnimationFrame(frame);
    sample.onInput = (event) => {
      const start = performance.now();
      const input = {
        atMs: start - sample.startedAt,
        target:
          event.target?.getAttribute?.("aria-label") || event.target?.tagName,
        trusted: event.isTrusted,
        focused: document.hasFocus(),
        visibility: document.visibilityState,
      };
      sample.inputFrames.push(input);
      requestAnimationFrame(() => {
        input.nextAnimationFrameMs = performance.now() - start;
        input.frameFocused = document.hasFocus();
        input.frameVisibility = document.visibilityState;
        sample.inputPaint.push(input.nextAnimationFrameMs);
      });
    };
    sample.onFocus = () =>
      sample.focusChanges.push({
        atMs: performance.now() - sample.startedAt,
        focused: document.hasFocus(),
        visibility: document.visibilityState,
      });
    sample.onFocus();
    window.addEventListener("focus", sample.onFocus);
    window.addEventListener("blur", sample.onFocus);
    document.addEventListener("visibilitychange", sample.onFocus);
    document.addEventListener("input", sample.onInput, true);
    if (typeof PerformanceObserver !== "undefined")
      try {
        sample.observer = new PerformanceObserver((list) =>
          sample.longTasks.push(...list.getEntries().map((e) => e.duration)),
        );
        sample.observer.observe({ entryTypes: ["longtask"] });
      } catch {}
  });
  const start = performance.now();
  await rpc("thread.send", {
    thread: "performance-audit",
    prompt:
      Array.from(
        { length: count },
        (_, i) => `!tool Read performance-${i}`,
      ).join("\n") + "\nPERFORMANCE_STREAM_FINISHED",
  });
  await expect(
    page
      .locator(".block-assistant")
      .getByText("PERFORMANCE_STREAM_FINISHED", { exact: true }),
  ).toBeVisible({ timeout: 120000 });
  const streamMs = performance.now() - start;
  const input = page.getByRole("textbox", {
    name: "Message performance-audit directly",
    exact: true,
  });
  const inputSamples = [];
  for (let i = 0; i < 12; i++) {
    const at = performance.now();
    await input.fill("Draft remains responsive " + i);
    inputSamples.push(performance.now() - at);
  }
  const inputGeometry = await page.evaluate(() => {
    const button = document.querySelector('button[aria-label="Send message"]');
    return {
      bottom: button.getBoundingClientRect().bottom,
      height: window.visualViewport?.height || innerHeight,
    };
  });
  expect(inputGeometry.bottom).toBeLessThanOrEqual(inputGeometry.height + 1);
  const rpcSamples = [];
  for (let i = 0; i < 20; i++) {
    const at = performance.now();
    await rpc("hub.info");
    rpcSamples.push(performance.now() - at);
  }
  await page.evaluate(async () => {
    const scroller = document.querySelector(".conversation-scroll");
    for (let i = 0; i < 40; i++) {
      scroller.scrollTop = i % 2 ? scroller.scrollHeight : 0;
      await new Promise(requestAnimationFrame);
    }
  });
  const browser = await page.evaluate(() => {
    const s = window.__twrPerf;
    s.running = false;
    s.observer?.disconnect();
    document.removeEventListener("input", s.onInput, true);
    window.removeEventListener("focus", s.onFocus);
    window.removeEventListener("blur", s.onFocus);
    document.removeEventListener("visibilitychange", s.onFocus);
    return {
      inputPaint: s.inputPaint,
      inputFrames: s.inputFrames,
      focusChanges: s.focusChanges,
      rendererDOMContentLoadedMs:
        performance.getEntriesByType("navigation")[0]?.domContentLoadedEventEnd,
      frames: s.frames,
      longTasks: s.longTasks,
      heap: performance.memory?.usedJSHeapSize,
      dom: document.getElementsByTagName("*").length,
      renderedBlocks: document.querySelectorAll(".transcript-block").length,
      viewport: { width: innerWidth, height: innerHeight },
      userAgent: navigator.userAgent,
    };
  });
  const pct = (values, p) =>
    [...values].sort((a, b) => a - b)[
      Math.min(values.length - 1, Math.floor(values.length * p))
    ] || 0;
  const report = {
    ...qaSource(),
    label,
    date: new Date().toISOString(),
    toolCalls: count,
    streamToVisibleMs: Math.round(streamMs),
    rpcMedianMs: Math.round(pct(rpcSamples, 0.5)),
    rpcP95Ms: Math.round(pct(rpcSamples, 0.95)),
    inputPaintP95Ms: Math.round(pct(browser.inputPaint, 0.95)),
    inputFrameMedianMs: Math.round(pct(browser.inputPaint, 0.5)),
    inputEventCount: browser.inputFrames.length,
    inputFrameSamplesCompleted: browser.inputPaint.length,
    inputMeasurement:
      "input event to next requestAnimationFrame; not paint or INP",
    rendererDOMContentLoadedMs: Math.round(
      browser.rendererDOMContentLoadedMs || 0,
    ),
    inputMedianMs: Math.round(pct(inputSamples, 0.5)),
    inputP95Ms: Math.round(pct(inputSamples, 0.95)),
    frameP95Ms: Math.round(pct(browser.frames, 0.95)),
    frameMaxMs: Math.round(Math.max(0, ...browser.frames)),
    longTasks: browser.longTasks.length,
    longestTaskMs: Math.round(Math.max(0, ...browser.longTasks)),
    heapMiB: browser.heap ? Math.round(browser.heap / 1048576) : null,
    domNodes: browser.dom,
    renderedBlocks: browser.renderedBlocks,
    viewport: browser.viewport,
    userAgent: browser.userAgent,
    inputToAnimationFrameSamplesMs: browser.inputPaint,
    inputFrameDiagnostics: browser.inputFrames,
    frameSamplesMs: browser.frames,
    focusChanges: browser.focusChanges,
  };
  if (application) {
    const metrics = await application.evaluate(({ app }) =>
      app
        .getAppMetrics()
        .map((m) => ({ type: m.type, rssKiB: m.memory.workingSetSize })),
    );
    report.electronResidentMiB = Math.round(
      metrics.reduce((sum, m) => sum + m.rssKiB, 0) / 1024,
    );
    report.electronProcesses = metrics;
  }
  expect(report.renderedBlocks).toBeLessThan(80);
  // Verify virtualization with all 1,000 tool calls visible as well as the default view.
  const tools = page.locator('button[aria-label="Toggle tool calls"]');
  await tools.evaluate((button) => button.click());
  await expect
    .poll(() => page.locator(".block-tool").count())
    .toBeGreaterThan(0);
  report.detailedRenderedBlocks = await page
    .locator(".transcript-block")
    .count();
  expect(report.detailedRenderedBlocks).toBeLessThan(80);
  await tools.evaluate((button) => button.click());
  mkdirSync("test-results", { recursive: true });
  writeFileSync(
    `test-results/performance-${label}.json`,
    JSON.stringify(report, null, 2) + "\n",
  );
  await input.fill("");
  const {
    inputToAnimationFrameSamplesMs,
    inputFrameDiagnostics,
    frameSamplesMs,
    focusChanges,
    ...summary
  } = report;
  console.log("PERFORMANCE", JSON.stringify(summary));
  return report;
}
