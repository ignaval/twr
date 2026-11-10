// Ends what a QA fixture started under its own TWR_HOME, before the fixture deletes it.
import { execFileSync } from "node:child_process";
import { realpathSync } from "node:fs";

/**
 * Stops the fixture's hubs (`hub stop` in each of `workspaces`), then the linked-runner
 * service a release hub starts for its twr home (docs/runner-links.md), which outlives the
 * hubs. `twr runner stop` stops that service but leaves its launchd job loaded on macOS, so
 * the jobs whose files lie inside this home are then booted out too. It only ever acts on
 * `env.TWR_HOME`, and it never throws: call it on failure paths too.
 */
export function stopFixture(
  twr,
  env,
  { workspaces = ["default"], timeout = 15000 } = {},
) {
  const home = env?.TWR_HOME;
  if (!home) throw new Error("stopFixture needs the fixture's own TWR_HOME");
  const run = (args) => {
    try {
      execFileSync(twr, args, { env, stdio: "ignore", timeout });
    } catch {}
  };
  for (const workspace of workspaces)
    run([...(workspace === "default" ? [] : ["-w", workspace]), "hub", "stop"]);
  run(["runner", "stop"]);
  const removed = removeServiceJobs(home);
  if (removed.length)
    console.log(
      `Removed the fixture's runner service job ${removed.join(", ")}`,
    );
}

/** Boots out the launchd jobs (`tools.twr.box.*`) whose plist is inside `home` (macOS). */
export function removeServiceJobs(home) {
  if (process.platform !== "darwin" || !home) return [];
  const roots = new Set([home]);
  try {
    roots.add(realpathSync(home));
  } catch {}
  const inside = (path) =>
    [...roots].some((root) => path.startsWith(root.replace(/\/+$/, "") + "/"));
  const domain = `gui/${process.getuid()}`;
  let labels;
  try {
    labels = execFileSync("/bin/launchctl", ["list"], { encoding: "utf8" })
      .split("\n")
      .map((line) => line.trim().split(/\s+/)[2])
      .filter((label) => label?.startsWith("tools.twr.box."));
  } catch {
    return [];
  }
  const removed = [];
  for (const label of labels) {
    let printed;
    try {
      printed = execFileSync(
        "/bin/launchctl",
        ["print", `${domain}/${label}`],
        {
          encoding: "utf8",
          stdio: ["ignore", "pipe", "ignore"],
        },
      );
    } catch {
      continue;
    }
    const path = /^\s+path = (.+)$/m.exec(printed)?.[1]?.trim();
    if (!path || !inside(path)) continue;
    try {
      execFileSync("/bin/launchctl", ["bootout", `${domain}/${label}`], {
        stdio: "ignore",
      });
      removed.push(label);
    } catch {}
  }
  return removed;
}
