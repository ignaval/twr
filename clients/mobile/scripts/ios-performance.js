const wait = async (check, limit = 120000) => {
  const deadline = performance.now() + limit;
  while (performance.now() < deadline) {
    if (await check()) return;
    await new Promise((r) => setTimeout(r, 30));
  }
  throw new Error(
    "iOS performance fixture timed out: " +
      check.toString() +
      " | " +
      document.body.innerText.slice(-2000),
  );
};
const rpc = (method, params = {}) => window.twr.rpc(method, params);
const probeStarted = performance.now();
await rpc("thread.new", {
  project: "demo",
  name: "performance-audit",
  runtime: "fake",
});
const worker = await rpc("thread.get", { thread: "performance-audit" });
if (typeof worker.id !== "string" || !worker.id)
  throw new Error("Performance worker identity is missing");
const marker = "PERFORMANCE_STREAM_FINISHED";
const expectedCounts = {
  toolCalls: 1000,
  toolResults: 1000,
  textDeltas: 1,
  textFinals: 1,
};
const eventCounts = {
  toolCalls: 0,
  toolResults: 0,
  textDeltas: 0,
  textFinals: 0,
};
const streamEvents = [];
const observedSeqs = new Set();
const measurementErrors = [];
const transitions = [];
let duplicateEvents = 0;
let workloadStart;
const stateEncoding = {
  events: 0,
  totalBytes: 0,
  maxBytes: 0,
  measurementMs: 0,
};
const stopMeasuring = window.twr.onEvent((message) => {
  if (workloadStart !== undefined && message.type === "state") {
    const at = performance.now();
    const bytes = new TextEncoder().encode(JSON.stringify(message)).byteLength;
    stateEncoding.events++;
    stateEncoding.totalBytes += bytes;
    stateEncoding.maxBytes = Math.max(stateEncoding.maxBytes, bytes);
    stateEncoding.measurementMs += performance.now() - at;
  }
  if (message.type === "resync" || message.type === "connection") {
    if (transitions.length < 32)
      transitions.push({
        type: message.type,
        scope: message.scope ?? null,
        connected: message.connected ?? null,
        atMs: performance.now() - probeStarted,
      });
  }
  const event = message.event;
  if (
    workloadStart === undefined ||
    message.type !== "thread" ||
    event?.thread !== worker.id
  )
    return;
  let category;
  if (event.kind === "agent.tool_call") category = "toolCalls";
  else if (event.kind === "agent.tool_result") category = "toolResults";
  else if (event.kind === "agent.text" && event.data?.text === marker) {
    if (event.data.final === true) category = "textFinals";
    else if (event.data.delta === true) category = "textDeltas";
  }
  if (!category) return;
  const server = Date.parse(event.ts);
  if (
    !Number.isSafeInteger(event.seq) ||
    event.seq <= 0 ||
    !Number.isFinite(server)
  ) {
    if (measurementErrors.length < 4)
      measurementErrors.push("Invalid event sequence or timestamp");
    return;
  }
  if (observedSeqs.has(event.seq)) {
    duplicateEvents++;
    return;
  }
  if (streamEvents.length >= 2002) {
    if (measurementErrors.length < 4)
      measurementErrors.push("Unexpected extra fixture events");
    return;
  }
  observedSeqs.add(event.seq);
  eventCounts[category]++;
  streamEvents.push({
    seq: event.seq,
    kind: event.kind,
    delta: event.data?.delta === true,
    final: event.data?.final === true,
    server,
    received: performance.now() - workloadStart,
  });
});
const readiness = { workerId: worker.id, frameFlushes: 2 };
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
// The empty transcript is rendered only after initial history and desktop.subscribe
// finish. Flush passive effects first, then require that positive ready state.
const transcriptState = () => ({
  composer:
    document.querySelector(".composer textarea")?.getAttribute("aria-label") ??
    null,
  loading: !!document.querySelector(".conversation-scroll .transcript-loading"),
  error: !!document.querySelector(".conversation-scroll .error-banner"),
  emptyTitle:
    document.querySelector(".conversation-scroll .empty h2")?.textContent ??
    null,
  emptyVisible: !!document
    .querySelector(".conversation-scroll .empty")
    ?.getClientRects().length,
  blocks: document.querySelectorAll(".conversation-scroll .transcript-block")
    .length,
});
readiness.beforeFrames = transcriptState();
await new Promise(requestAnimationFrame);
await new Promise(requestAnimationFrame);
await wait(() => {
  const state = transcriptState();
  return (
    state.composer === `Message ${worker.name} directly` &&
    !state.loading &&
    !state.error &&
    state.emptyTitle === worker.name &&
    state.emptyVisible &&
    state.blocks === 0
  );
});
readiness.ready = transcriptState();
readiness.readyAtMs = Math.round(performance.now() - probeStarted);
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
workloadStart = performance.now();
let sendAckMs, streamToVisibleMs;
const finalTextEvent = () =>
  streamEvents.find((event) => event.kind === "agent.text" && event.final);
const exactAssistantText = () =>
  [...document.querySelectorAll(".block-assistant .markdown")].some(
    (node) => node.textContent.trim() === marker,
  );
// The stream's end. On a CI runner the debug hub, the gateway and the Simulator share a few
// cores: the hub's own timestamps of the 2002 fixture events have spanned 1 s to 50 s there
// (0.2 s on a laptop), and the app received them as they came. A fixed limit fails a run that
// is only slow. So it waits as long as events keep arriving: it fails when none has come for
// STREAM_STALL_MS while the final one is still missing (a stall), or after STREAM_LIMIT_MS in
// all, saying how far the stream got.
const STREAM_STALL_MS = 60000;
const STREAM_LIMIT_MS = 300000;
const streamEnd = async () => {
  const started = performance.now();
  let seen = -1,
    progressAt = started;
  for (;;) {
    if (measurementErrors.length) throw new Error(measurementErrors.join("; "));
    if (!!finalTextEvent() && exactAssistantText()) return;
    const now = performance.now();
    if (streamEvents.length !== seen) {
      seen = streamEvents.length;
      progressAt = now;
    }
    const stalled = !finalTextEvent() && now - progressAt > STREAM_STALL_MS;
    if (stalled || now - started > STREAM_LIMIT_MS) {
      const first = streamEvents[0],
        last = streamEvents.at(-1);
      throw new Error(
        "iOS performance fixture timed out: " +
          (stalled
            ? `no fixture event for ${STREAM_STALL_MS / 1000} s`
            : `the stream took over ${STREAM_LIMIT_MS / 1000} s`) +
          " " +
          JSON.stringify({
            eventCounts,
            uniqueEvents: streamEvents.length,
            duplicateEvents,
            serverSpanMs: last ? last.server - first.server : null,
            firstReceivedMs: first ? Math.round(first.received) : null,
            lastReceivedMs: last ? Math.round(last.received) : null,
            finalEvent: !!finalTextEvent(),
            transitions,
          }) +
          " | " +
          document.body.innerText.slice(-2000),
      );
    }
    await new Promise((r) => setTimeout(r, 30));
  }
};
try {
  // Observe the DOM independently of the RPC reply, which may arrive later.
  const observed = streamEnd().then(() => {
    streamToVisibleMs = Math.round(performance.now() - workloadStart);
  });
  const acknowledged = rpc("thread.send", {
    thread: worker.id,
    prompt:
      Array.from(
        { length: 1000 },
        (_, i) => `!tool Read performance-${i}`,
      ).join("\n") +
      "\n" +
      marker,
  }).then(() => {
    sendAckMs = Math.round(performance.now() - workloadStart);
  });
  await Promise.all([observed, acknowledged]);
  if (
    measurementErrors.length ||
    streamEvents.length !== 2002 ||
    Object.entries(expectedCounts).some(
      ([key, value]) => eventCounts[key] !== value,
    )
  )
    throw new Error(
      "Incomplete unique live-stream measurement: " +
        JSON.stringify({
          expectedCounts,
          eventCounts,
          uniqueEvents: streamEvents.length,
          duplicateEvents,
          measurementErrors,
          readiness,
          transitions,
          stateEncoding,
        }),
    );
} catch (error) {
  running = false;
  throw error;
} finally {
  stopMeasuring();
}
const firstEvent = streamEvents[0],
  lastEvent = streamEvents.at(-1);
const serverStreamSpanMs = lastEvent.server - firstEvent.server;
const receivedStreamSpanMs = Math.round(
  lastEvent.received - firstEvent.received,
);
const finalEventToDomObservationMs = Math.round(
  streamToVisibleMs - finalTextEvent().received,
);
const input = document.querySelector(".composer textarea");
const storageTimes = [];
const originalStorageWrite = Storage.prototype.setItem;
Storage.prototype.setItem = function (key, value) {
  const at = performance.now();
  try {
    return originalStorageWrite.call(this, key, value);
  } finally {
    storageTimes.push(performance.now() - at);
  }
};
try {
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
} finally {
  Storage.prototype.setItem = originalStorageWrite;
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
  assistantCompletion: !!finalTextEvent() && exactAssistantText(),
  measurementDescription:
    "Exact final event to DOM text observation, polled every 30ms; includes UI batching/scheduling, not paint or INP. Server/received spans cover 2002 unique fixture events.",
  inputMeasurement: "Input to next requestAnimationFrame, not paint or INP.",
  sendAckMs,
  streamToVisibleMs,
  serverStreamSpanMs,
  receivedStreamSpanMs,
  streamMessageCount: streamEvents.length,
  streamEventCounts: eventCounts,
  duplicateStreamEvents: duplicateEvents,
  readiness,
  streamTransitions: transitions,
  stateEncoding,
  streamEventTimings: streamEvents,
  rpcMedianMs: pct(rpcTimes, 0.5),
  rpcP95Ms: pct(rpcTimes, 0.95),
  inputPaintP95Ms: pct(inputPaint, 0.95),
  inputPaintMedianMs: pct(inputPaint, 0.5),
  inputPaintSamplesMs: inputPaint.map(Math.round),
  storageWriteMaxMs: Math.round(Math.max(0, ...storageTimes) * 10) / 10,
  finalEventToDomObservationMs,
  frameP95Ms: pct(frames, 0.95),
  heapMiB: null,
  domNodes: document.getElementsByTagName("*").length,
  renderedBlocks: document.querySelectorAll(".transcript-block").length,
  viewport: { width: innerWidth, height: innerHeight },
  userAgent: navigator.userAgent,
};
if (report.renderedBlocks >= 80)
  throw new Error("Unbounded transcript rendering");
const toolToggle = document.querySelector(
  'button[aria-label="Toggle tool calls"]',
);
toolToggle.click();
await wait(() => document.querySelectorAll(".block-tool").length > 0, 15000);
report.detailedRenderedBlocks =
  document.querySelectorAll(".transcript-block").length;
if (report.detailedRenderedBlocks >= 80)
  throw new Error("Unbounded detailed transcript rendering");
toolToggle.click();
await new Promise(requestAnimationFrame);
const send = document
  .querySelector('button[aria-label="Send message"]')
  .getBoundingClientRect();
const viewport = window.visualViewport;
if (
  send.bottom >
  (viewport?.height || innerHeight) + (viewport?.offsetTop || 0) + 1
)
  throw new Error(
    "Composer outside the visible viewport: " +
      JSON.stringify({
        send: send.toJSON(),
        width: innerWidth,
        height: innerHeight,
        viewport: viewport && {
          width: viewport.width,
          height: viewport.height,
          offsetTop: viewport.offsetTop,
          scale: viewport.scale,
        },
        root: document.documentElement.style.cssText,
        layout: await window.Capacitor.Plugins.TwrLayout.current(),
      }),
  );
return report;
