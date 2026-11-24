import type { EngineHub } from "./types";

/** A build id (`twr_core::build_id_of`): the first 16 hex digits of the engine's SHA-256. */
export const BUILD_ID_LEN = 16;
export const BUILD_ID = /^[a-f0-9]{16}$/;

/** The build id of an engine whose SHA-256 is `sha256`, if it is one. */
export function buildIdOf(sha256: unknown): string | undefined {
  if (typeof sha256 !== "string" || !/^[a-f0-9]{64}$/.test(sha256))
    return undefined;
  return sha256.slice(0, BUILD_ID_LEN);
}

/** The engine the app bundles: its version (`desktop.paths`) and build id, when known. */
export interface Engine {
  version: string;
  build?: string;
}

/**
 * How the engine a hub runs (its `hub.info`) differs from the app's: `"version"` when the
 * versions differ, `"build"` when they are the same but the builds differ (a rebuild of one
 * version, such as a distribution package built from a later commit), else `undefined`.
 * A hub or an app without a build id (an older hub) is compared by version alone.
 */
export function engineDifference(
  app: Engine,
  hub: { version?: unknown; build?: unknown },
): EngineHub["difference"] | undefined {
  if (typeof hub.version !== "string" || !hub.version) return undefined;
  if (hub.version !== app.version) return "version";
  const build = typeof hub.build === "string" ? hub.build : undefined;
  if (
    app.build &&
    build &&
    BUILD_ID.test(build) &&
    BUILD_ID.test(app.build) &&
    build !== app.build
  )
    return "build";
  return undefined;
}

/** A build id as the notice shows it: its first 8 digits. */
const shortBuild = (build?: string) => (build ? build.slice(0, 8) : "unknown");

/** `a`, `a and b`, `a, b and c`. */
function listNames(names: string[]) {
  return names.length < 2
    ? names.join("")
    : `${names.slice(0, -1).join(", ")} and ${names.at(-1)}`;
}

/**
 * The notice's words for the hubs whose engine differs from the app's (`engineHubs`, and
 * the current workspace's from its connection), or `undefined` when there are none.
 */
export function engineNotice(
  app: Engine,
  hubs: EngineHub[],
  current: string,
): string | undefined {
  if (!hubs.length) return undefined;
  const mine = hubs.find((hub) => hub.workspace === current);
  const others = hubs.filter((hub) => hub.workspace !== current);
  const parts: string[] = [];
  if (mine)
    parts.push(
      mine.difference === "version"
        ? `The app bundles twr ${app.version}; this hub still runs ${mine.version}.`
        : `The app bundles a different build of twr ${app.version} (${shortBuild(app.build)}) than this hub runs (${shortBuild(mine.build)}).`,
    );
  if (others.length) {
    const what = others.every((hub) => hub.difference === "build")
      ? "a different build of twr"
      : "a different twr version";
    const names = listNames(others.map((hub) => hub.workspace));
    parts.push(
      others.length === 1
        ? `The hub of workspace ${names} also runs ${what} than the app bundles.`
        : `The hubs of workspaces ${names} also run ${what} than the app bundles.`,
    );
    if (!mine) parts[0] = parts[0].replace(" also ", " ");
  }
  parts.push(
    hubs.length > 1
      ? "Stop them and reconnect when you’re ready to use the app’s engine."
      : "Stop and reconnect when you’re ready to use the app’s engine.",
  );
  return parts.join(" ");
}
