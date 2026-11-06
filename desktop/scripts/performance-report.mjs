import { readFileSync, writeFileSync, existsSync } from "node:fs";
const read = (name) =>
  JSON.parse(readFileSync(`test-results/performance-${name}.json`, "utf8"));
const platforms = [
  ["mac-packaged", "macOS · packaged"],
  ["android-phone", "Android · phone emulator"],
  ["android-fold", "Android · fold emulator"],
  ["ios-phone", "iOS · phone simulator"],
  ["ios-fold", "iOS · Duo simulator"],
  ["ios-tablet", "iOS · tablet simulator"],
  ["linux-hyprland", "Linux · QEMU / Hyprland"],
]
  .filter(([key]) => existsSync(`test-results/performance-${key}.json`))
  .map(([key, name]) => ({ ...read(key), name, key }))
  .filter(
    (sample) =>
      !sample.key.startsWith("ios-") || sample.assistantCompletion === true,
  );
const before = read("mac-before"),
  after = read("mac-after");
const esc = (s) =>
  String(s)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll('"', "&quot;");
const bar = (name, oldValue, newValue, max) =>
  `<div class="comparison"><b>${name}</b><div class="bar-row"><span>Before</span><div class="track"><i class="old" style="width:${(oldValue / max) * 100}%"></i></div><strong>${oldValue} ms</strong></div><div class="bar-row"><span>After</span><div class="track"><i style="width:${Math.max(1, (newValue / max) * 100)}%"></i></div><strong>${newValue} ms</strong></div></div>`;
const html = `<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>twr performance audit</title><style>
:root{color-scheme:light dark;--report-fg:var(--foreground,light-dark(#253b35,#e4eee9));--report-muted:var(--muted-foreground,light-dark(#64796f,#a1b4a8));--report-line:var(--border,light-dark(#dde7e0,#34483e));--report-accent:var(--chart-1,light-dark(#337766,#8dd8b8));--report-old:var(--chart-2,light-dark(#b0beb6,#64796f))}body{font:13px/1.6 var(--font-sans,system-ui),sans-serif;color:var(--report-fg);margin:0}p{margin:0 0 18px;color:var(--report-muted)}.charts{display:grid;grid-template-columns:repeat(auto-fit,minmax(260px,1fr));gap:28px;margin:0 0 24px}.comparison>b{display:block;margin-bottom:12px;font-size:14px}.bar-row{display:grid;grid-template-columns:45px 1fr 64px;gap:10px;align-items:center;margin:8px 0}.bar-row>span{font-size:12px;color:var(--report-muted)}.bar-row>strong{text-align:right;font:12px ui-monospace,monospace}.track{height:20px;background:color-mix(in srgb,var(--report-line) 35%,transparent)}i{display:block;height:20px;background:var(--report-accent)}i.old{background:var(--report-old)}.table{overflow:auto}table{width:100%;border-collapse:collapse;white-space:nowrap;font-variant-numeric:tabular-nums}th,td{padding:10px 12px;border-bottom:1px solid var(--report-line);text-align:right}th:first-child,td:first-child{text-align:left;padding-left:0}th{font-size:11px;color:var(--report-muted);font-weight:600}small{display:block;margin-top:18px;color:var(--report-muted);font-size:11px}code{font:11px ui-monospace,monospace}
</style></head><body><p>twr · measured 6 November 2026. Real app clients, disposable Rust hubs and a deterministic local agent. Lower is better.</p><div class="charts">${bar("Mac: 1,000 tool events to visible transcript", before.streamToVisibleMs, after.streamToVisibleMs, 1000)}${bar("Mac: median hub RPC", before.rpcMedianMs, after.rpcMedianMs, 140)}</div><p>Before/after uses the same Mac development harness and 420 × 480 viewport. Improvements: pooled authenticated hub connections, memoized markdown, bounded executable hashing. Current packaged and emulator results follow.</p><div class="table"><table><thead><tr><th>Platform</th><th>1,000 events</th><th>RPC p95</th><th>Input → paint p95</th><th>Frame p95</th><th>Rendered blocks</th></tr></thead><tbody>${platforms.map((p) => `<tr><td>${esc(p.name)}</td><td>${p.streamToVisibleMs} ms</td><td>${p.rpcP95Ms} ms</td><td>${p.inputPaintP95Ms} ms</td><td>${p.frameP95Ms} ms</td><td>${p.renderedBlocks}</td></tr>`).join("")}</tbody></table></div><small>Single-run diagnostics, not controlled benchmarks. Android runs on ARM64 emulators; Linux runs x86_64 QEMU emulation with a software GPU and Hyprland at 125% scale. Mobile input measurements include the keyboard-visible viewport where available. No real provider/network latency, battery or physical-device power measurements. Native signing and APNs/FCM delivery need credentials. Missing platforms have not been measured.</small></body></html>`;
writeFileSync("test-results/performance.html", html);
console.log("Wrote test-results/performance.html");
