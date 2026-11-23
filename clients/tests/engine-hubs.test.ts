import { describe, expect, it, vi } from "vitest";
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
  const infos: Record<string, any> = {
    default: {
      workspace: "default",
      version: "0.6.0",
      build: "85ab7b43eecbe2ec",
    },
    twr: { workspace: "twr", version: "0.6.0", build: "85ab7b43eecbe2ec" },
    fresh: { workspace: "fresh", version: "0.6.0", build: app.build },
    old: { workspace: "old", version: "0.5.2" },
    impostor: { workspace: "default", version: "0.5.2" },
  };
  const reader = (asked: string[], closed: string[]) => (name: string) => {
    asked.push(name);
    return {
      call: async (method: string) => {
        expect(method).toBe("hub.info");
        if (name === "hung") return new Promise(() => {});
        if (name === "broken") throw new Error("connection refused");
        return infos[name];
      },
      close: () => void closed.push(name),
    };
  };

  it("lists every running hub on another engine, and asks only running hubs", async () => {
    const asked: string[] = [];
    const closed: string[] = [];
    const hubs = await engineHubs(
      [
        { name: "default", hub: "running" },
        { name: "twr", hub: "running" },
        { name: "fresh", hub: "running" },
        { name: "old", hub: "running" },
        { name: "idle", hub: "stopped" },
        { name: "busy", hub: "busy" },
      ],
      app,
      reader(asked, closed),
    );
    expect(hubs).toEqual([
      {
        workspace: "default",
        difference: "build",
        version: "0.6.0",
        build: "85ab7b43eecbe2ec",
      },
      {
        workspace: "twr",
        difference: "build",
        version: "0.6.0",
        build: "85ab7b43eecbe2ec",
      },
      { workspace: "old", difference: "version", version: "0.5.2" },
    ]);
    expect(asked).toEqual(["default", "twr", "fresh", "old"]);
    expect(closed.sort()).toEqual([...asked].sort());
  });

  it("leaves out a hub that serves another workspace, fails, or does not answer", async () => {
    vi.useFakeTimers();
    try {
      const closed: string[] = [];
      const pending = engineHubs(
        ["impostor", "broken", "hung"].map((name) => ({
          name,
          hub: "running",
        })),
        app,
        reader([], closed),
        3000,
      );
      await vi.advanceTimersByTimeAsync(3000);
      expect(await pending).toEqual([]);
      expect(closed.sort()).toEqual(["broken", "hung", "impostor"]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("refuses a listing that is not one", async () => {
    await expect(engineHubs({}, app, reader([], []))).rejects.toThrow(
      "Invalid workspace listing",
    );
    await expect(
      engineHubs([{ name: "../x", hub: "running" }], app, reader([], [])),
    ).rejects.toThrow();
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
