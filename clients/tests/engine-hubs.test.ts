import { describe, expect, it } from "vitest";
import { createHash } from "node:crypto";
import { mkdtempSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { fileDigest } from "../desktop/engines";
import { engineHubs } from "../desktop/engine-hubs";
import {
  buildIdOf,
  engineDifference,
  engineNotice,
} from "../shared/engineHubs";

const app = { version: "0.6.0", build: "463a2ed0aaaaaaaa" };

describe("engineDifference", () => {
  it("tells a rebuild of the same version from the app's engine by its build", () => {
    expect(
      engineDifference(app, { version: "0.6.0", build: "85ab7b43eecbe2ec" }),
    ).toBe("build");
    expect(
      engineDifference(app, { version: "0.6.0", build: "463a2ed0aaaaaaaa" }),
    ).toBeUndefined();
  });
  it("says version when the versions differ, whatever the builds", () => {
    expect(
      engineDifference(app, { version: "0.5.2", build: "463a2ed0aaaaaaaa" }),
    ).toBe("version");
    expect(engineDifference(app, { version: "0.6.1" })).toBe("version");
  });
  it("compares by version alone without both builds", () => {
    // An older hub has no `build`; an engine whose digest could not be read has none.
    expect(engineDifference(app, { version: "0.6.0" })).toBeUndefined();
    expect(
      engineDifference(
        { version: "0.6.0" },
        { version: "0.6.0", build: "85ab7b43eecbe2ec" },
      ),
    ).toBeUndefined();
    expect(
      engineDifference(app, { version: "0.6.0", build: "not a build" }),
    ).toBeUndefined();
    expect(engineDifference(app, {})).toBeUndefined();
  });
});

describe("buildIdOf", () => {
  it("is the hub's build id of the same file: the first 16 digits of its SHA-256", () => {
    const dir = mkdtempSync(join(tmpdir(), "twr-build-"));
    const file = join(dir, "twr");
    writeFileSync(file, "engine bytes");
    const sha = createHash("sha256").update("engine bytes").digest("hex");
    expect(buildIdOf(fileDigest(file))).toBe(sha.slice(0, 16));
    expect(buildIdOf("short")).toBeUndefined();
    expect(buildIdOf(undefined)).toBeUndefined();
  });
});

describe("engineHubs", () => {
  const running = (name: string, info: Record<string, unknown>) => ({
    name,
    hub: "running",
    threads: 0,
    running: 0,
    waiting: 0,
    current: false,
    ...info,
  });
  it("lists every running hub on another engine, from the listing alone", () => {
    const old = { version: "0.6.0", build: "85ab7b43eecbe2ec" };
    expect(
      engineHubs(
        [
          running("default", old),
          running("twr", old),
          running("fresh", { version: "0.6.0", build: app.build }),
          running("older", { version: "0.5.2" }),
          // Not running, or not answered: nothing to compare.
          { name: "idle", hub: "stopped" },
          { name: "busy", hub: "busy", version: "0.5.2" },
          running("mute", {}),
        ],
        app,
      ),
    ).toEqual([
      { workspace: "default", difference: "build", ...old },
      { workspace: "twr", difference: "build", ...old },
      { workspace: "older", difference: "version", version: "0.5.2" },
    ]);
  });
  it("keys a renamed workspace's hub by its id, named as shown", () => {
    const listing = [
      { id: "a", name: "acme", hub: "running", version: "0.5.2" },
      { id: "default", name: "default", hub: "running", version: "0.5.2" },
    ];
    const hubs = engineHubs(listing, app);
    expect(hubs).toEqual([
      { workspace: "a", name: "acme", difference: "version", version: "0.5.2" },
      { workspace: "default", difference: "version", version: "0.5.2" },
    ]);
    // The app's workspace is the id: the current one is told apart by it.
    expect(engineNotice(app, hubs, "default")).toBe(
      "The app bundles twr 0.6.0; this hub still runs 0.5.2. The hub of workspace acme also runs a different twr version than the app bundles. Stop them and reconnect when you’re ready to use the app’s engine.",
    );
  });
  it("drops a build that is not one", () => {
    expect(
      engineHubs([running("default", { version: "0.5.2", build: "x" })], app),
    ).toEqual([
      { workspace: "default", difference: "version", version: "0.5.2" },
    ]);
  });
  it("refuses a listing that is not one", () => {
    expect(() => engineHubs({}, app)).toThrow("Invalid workspace listing");
    expect(() =>
      engineHubs([running("../x", { version: "0.5.2" })], app),
    ).toThrow();
  });
});

describe("engineNotice", () => {
  const build = (workspace: string) => ({
    workspace,
    difference: "build" as const,
    version: "0.6.0",
    build: "85ab7b43eecbe2ec",
  });
  it("is nothing without hubs on another engine", () => {
    expect(engineNotice(app, [], "default")).toBeUndefined();
  });
  it("speaks of a build when only the build differs", () => {
    expect(engineNotice(app, [build("default")], "default")).toBe(
      "The app bundles a different build of twr 0.6.0 (463a2ed0) than this hub runs (85ab7b43). Stop and reconnect when you’re ready to use the app’s engine.",
    );
  });
  it("speaks of the version when it differs", () => {
    expect(
      engineNotice(
        app,
        [{ workspace: "default", difference: "version", version: "0.5.2" }],
        "default",
      ),
    ).toBe(
      "The app bundles twr 0.6.0; this hub still runs 0.5.2. Stop and reconnect when you’re ready to use the app’s engine.",
    );
  });
  it("names every other workspace's hub", () => {
    expect(
      engineNotice(
        app,
        [build("default"), build("twr"), build("api")],
        "default",
      ),
    ).toBe(
      "The app bundles a different build of twr 0.6.0 (463a2ed0) than this hub runs (85ab7b43). The hubs of workspaces twr and api also run a different build of twr than the app bundles. Stop them and reconnect when you’re ready to use the app’s engine.",
    );
    expect(
      engineNotice(
        app,
        [
          build("twr"),
          { workspace: "api", difference: "version", version: "0.5.2" },
        ],
        "default",
      ),
    ).toBe(
      "The hubs of workspaces twr and api run a different twr version than the app bundles. Stop them and reconnect when you’re ready to use the app’s engine.",
    );
  });
});
