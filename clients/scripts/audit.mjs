// CI's npm audit (ci.yml): every high or critical advisory in the whole tree fails, as
// `npm audit --audit-level=high` did, except the ones ALLOWED names. npm audit has no ignore
// option, so this reads its JSON report. Like deny.toml's `ignore`, every entry needs its
// reason, is scoped to one package, and should go as soon as a fixed release exists.
import { execFileSync } from "node:child_process";

const ALLOWED = [
  {
    id: "GHSA-fc5q-39xc-vjpf",
    package: "picomatch",
    // ReDoS on attacker-chosen glob patterns, every version through 4.0.7, no fixed release.
    // picomatch reaches the tree only through build and test tooling (vite, vitest,
    // electron-builder) running on twr's own patterns; neither app ships it.
    reason: "build-time only, no fixed release",
  },
];
const BLOCKING = new Set(["high", "critical"]);

let output;
try {
  output = execFileSync("npm", ["audit", "--json"], {
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
} catch (error) {
  // npm audit exits non-zero whenever it finds anything; the report is still on stdout.
  output = error.stdout;
  if (!output) throw error;
}
const report = JSON.parse(output);
if (report.error || !report.vulnerabilities)
  throw new Error(
    `npm audit failed: ${JSON.stringify(report.error ?? report)}`,
  );

// Advisories are the `via` objects; string `via` entries only point at another package
// whose own advisories are listed under that package.
const advisories = new Map();
for (const entry of Object.values(report.vulnerabilities))
  for (const via of entry.via)
    if (typeof via === "object" && BLOCKING.has(via.severity))
      advisories.set(via.url, via);

const idOf = (via) => via.url?.split("/").pop() ?? String(via.source);
const allowed = (via) =>
  ALLOWED.some((a) => a.id === idOf(via) && a.package === via.name);
const blocking = [...advisories.values()].filter((via) => !allowed(via));
for (const a of ALLOWED)
  if (![...advisories.values()].some((via) => idOf(via) === a.id))
    console.log(`note: ${a.id} (${a.package}) no longer reported; remove it.`);
for (const via of advisories.values())
  if (allowed(via))
    console.log(
      `allowed: ${idOf(via)} ${via.name} ${via.range}: ${via.title} (${ALLOWED.find((a) => a.id === idOf(via)).reason})`,
    );
if (blocking.length) {
  for (const via of blocking)
    console.error(
      `${via.severity}: ${idOf(via)} ${via.name} ${via.range}: ${via.title} (${via.url})`,
    );
  process.exit(1);
}
console.log("npm audit: no unallowed high or critical advisories");
