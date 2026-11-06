const wait = async (check, limit = 120000) => {
  const deadline = performance.now() + limit;
  while (performance.now() < deadline) {
    if (await check()) return;
    await new Promise((r) => setTimeout(r, 30));
  }
  throw new Error("iOS performance fixture timed out");
};
const rpc = (method, params = {}) => window.twr.rpc(method, params);
await rpc("thread.new", {
  project: "demo",
  name: "performance-audit",
  runtime: "fake",
});
const visible = (e) => e.getClientRects().length > 0;
[...document.querySelectorAll(".mobile-tabs button,.primary-nav button")]
  .find(
    (e) =>
      visible(e) &&
      (e.textContent === "Threads" || e.textContent.includes("Thread grid")),
  )
  .click();
await wait(() =>
  [...document.querySelectorAll(".thread-grid .thread-card")].some((e) =>
    e.textContent.includes("performance-audit"),
  ),
);
[...document.querySelectorAll(".thread-grid .thread-card")]
  .find((e) => e.textContent.includes("performance-audit"))
  .click();
await wait(() =>
  [...document.querySelectorAll("button")].some((e) =>
    e.textContent.includes("Talk to this thread"),
  ),
);
[...document.querySelectorAll("button")]
  .find((e) => e.textContent.includes("Talk to this thread"))
  .click();
await wait(
  () =>
    document.querySelector(".composer textarea")?.getAttribute("aria-label") ===
    "Message performance-audit directly",
);
const frames = [],
  inputPaint = [],
  rpcTimes = [];
let running = true,
  last = 0;
const frame = (now) => {
  if (last) frames.push(now - last);
  last = now;
  if (running) requestAnimationFrame(frame);
};
requestAnimationFrame(frame);
const streamEvents = [];
const stopMeasuring = window.twr.onEvent((message) => {
  if (
    message.type === "thread" &&
    message.event?.ts &&
    (message.event.kind === "agent.tool_call" ||
      message.event.kind === "agent.tool_result" ||
      (message.event.kind === "agent.text" &&
        message.event.data?.text === "PERFORMANCE_STREAM_FINISHED"))
  )
    streamEvents.push({
      server: Date.parse(message.event.ts),
      received: performance.now(),
    });
});
const start = performance.now();
await rpc("thread.send", {
  thread: "performance-audit",
  prompt:
    Array.from({ length: 1000 }, (_, i) => `!tool Read performance-${i}`).join(
      "\n",
    ) + "\nPERFORMANCE_STREAM_FINISHED",
});
const sendAckMs = Math.round(performance.now() - start);
await wait(() =>
  [...document.querySelectorAll(".block-assistant .markdown")].some(
    (node) => node.textContent.trim() === "PERFORMANCE_STREAM_FINISHED",
  ),
);
const streamToVisibleMs = Math.round(performance.now() - start);
stopMeasuring();
const firstEvent = streamEvents[0],
  lastEvent = streamEvents.at(-1);
const serverStreamSpanMs =
  firstEvent && lastEvent ? lastEvent.server - firstEvent.server : null;
const receivedStreamSpanMs =
  firstEvent && lastEvent
    ? Math.round(lastEvent.received - firstEvent.received)
    : null;
const input = document.querySelector(".composer textarea");
for (let i = 0; i < 12; i++) {
  const at = performance.now();
  Object.getOwnPropertyDescriptor(
    HTMLTextAreaElement.prototype,
    "value",
  ).set.call(input, "Responsive draft " + i);
  input.dispatchEvent(new Event("input", { bubbles: true }));
  await new Promise(requestAnimationFrame);
  inputPaint.push(performance.now() - at);
}
for (let i = 0; i < 20; i++) {
  const at = performance.now();
  await rpc("hub.info");
  rpcTimes.push(performance.now() - at);
}
const scroller = document.querySelector(".conversation-scroll");
for (let i = 0; i < 40; i++) {
  scroller.scrollTop = i % 2 ? scroller.scrollHeight : 0;
  await new Promise(requestAnimationFrame);
}
running = false;
const pct = (values, p) =>
  Math.round(
    [...values].sort((a, b) => a - b)[
      Math.min(values.length - 1, Math.floor(values.length * p))
    ] || 0,
  );
const report = {
  date: new Date().toISOString(),
  toolCalls: 1000,
  assistantCompletion: true,
  sendAckMs,
  streamToVisibleMs,
  serverStreamSpanMs,
  receivedStreamSpanMs,
  streamMessageCount: streamEvents.length,
  rpcMedianMs: pct(rpcTimes, 0.5),
  rpcP95Ms: pct(rpcTimes, 0.95),
  inputPaintP95Ms: pct(inputPaint, 0.95),
  frameP95Ms: pct(frames, 0.95),
  heapMiB: null,
  domNodes: document.getElementsByTagName("*").length,
  renderedBlocks: document.querySelectorAll(".transcript-block").length,
  viewport: { width: innerWidth, height: innerHeight },
  userAgent: navigator.userAgent,
};
if (report.renderedBlocks >= 80)
  throw new Error("Unbounded transcript rendering");
const send = document
  .querySelector('button[aria-label="Send message"]')
  .getBoundingClientRect();
if (send.bottom > (visualViewport?.height || innerHeight) + 1)
  throw new Error("Composer outside the visible viewport");
return report;
