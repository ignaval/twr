import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { CommandResult } from "../shared/CommandResult";
import { formatBytes, resultSummary } from "../shared/results";

describe("command results in words", () => {
  it("says what a thread change did, or the hub's reason nothing did", () => {
    expect(
      resultSummary("thread.set_mode", {
        thread_name: "api",
        mode: "plan",
        changed: true,
      })?.text,
    ).toBe("api now uses Plan only.");
    expect(
      resultSummary("thread.set_mode", {
        thread_name: "api",
        changed: false,
        message: "api is already in plan mode",
      })?.text,
    ).toBe("api is already in plan mode");
    expect(
      resultSummary("thread.set_model", {
        thread_name: "api",
        model: "opus",
        previous_model: "sonnet",
        effort: "high",
        previous_effort: "high",
        runtime_changed: false,
        account_changed: true,
        account: "work",
        changed: true,
        warning: "Applies on the next turn.",
      }),
    ).toEqual({
      text: "api now uses the opus model and the work account.",
      warning: "Applies on the next turn.",
    });
    expect(
      resultSummary("thread.rename", {
        thread_name: "web",
        previous: "api",
        changed: true,
      })?.text,
    ).toBe("Renamed api to web.");
    expect(
      resultSummary("thread.stop", {
        thread_name: "api",
        stopping: true,
        dropped_prompts: 2,
      })?.text,
    ).toBe("Stopping api. Dropped 2 queued messages.");
    expect(
      resultSummary("thread.park", {
        thread_name: "api",
        worktree_removed: true,
      })?.text,
    ).toBe("Parked api. Its worktree was removed; its branch stays.");
    expect(resultSummary("thread.resume", { thread_name: "api" })?.text).toBe(
      "Resumed api.",
    );
    expect(
      resultSummary("thread.compact", { thread_name: "api", state: "queued" })
        ?.text,
    ).toBe("api compacts after its current turn.");
    expect(
      resultSummary("thread.trim", {
        thread_name: "api",
        deleted: ["target/"],
        freed: 3 * 1024 * 1024,
        notes: [],
      }),
    ).toEqual({ text: "Freed 3.0 MB from api.", list: ["target/"] });
  });

  it("offers to open a new thread", () => {
    expect(
      resultSummary("thread.new", { id: "T1", name: "api" })?.openThread,
    ).toEqual({ id: "T1", name: "api" });
  });

  it("tabulates accounts, usage, storage and a box's agents", () => {
    expect(
      resultSummary("account.status", {
        runtimes: {
          codex: {
            accounts: [
              { name: "work", login: "logged_in" },
              { name: "gone", removed: true },
            ],
          },
          "claude-code": { accounts: [{ name: "me", native: true }] },
        },
      })?.table,
    ).toEqual({
      columns: ["Runtime", "Account", "Login"],
      rows: [
        ["codex", "work", "logged in"],
        ["claude-code", "me (native login)", "Not checked"],
      ],
    });
    const usage = resultSummary("usage.threads", {
      by: "project",
      rows: [{ label: "api", input: 1500, output: 20, turns: 3, cost: 1.5 }],
      total: { input: 1500, output: 20, turns: 3, cost: 1.5 },
    })!.table!;
    expect(usage.columns[0]).toBe("Project");
    expect(usage.rows.map((r) => r[0])).toEqual(["api", "Total"]);
    expect(usage.rows[0][4]).toBe("$1.50");
    expect(
      resultSummary("log.stats", {
        db: { bytes: 2048, wal: 0 },
        archive: null,
        retention_days: 30,
        next: { rows: 0 },
      })?.table?.rows,
    ).toEqual([
      ["Event log", "2.0 KB"],
      ["Kept for", "30 days"],
    ]);
    expect(
      resultSummary("box.agents", {
        box: "gpu",
        online: true,
        agents: [
          { runtime: "codex", found: true, version: "1.2", login: "logged_in" },
          { runtime: "claude-code", found: false, login: "unknown" },
        ],
      })?.table?.rows,
    ).toEqual([
      ["codex", "1.2", "logged in"],
      ["claude-code", "No", "unknown"],
    ]);
    expect(
      resultSummary("box.projects", {
        box: "gpu",
        projects: ["api"],
        repos: { api: "/src/api" },
      })?.list,
    ).toEqual(["api — /src/api"]);
  });

  it("says an update check's outcome with its next step", () => {
    const available = resultSummary("desktop.check_updates", {
      kind: "deb",
      automatic: false,
      state: "available",
      current: "0.5.0",
      latest: "0.6.0",
      releaseUrl: "https://example.com/r",
      download: { name: "twr.deb", url: "https://example.com/twr.deb" },
    })!;
    expect(available.text).toBe("twr 0.6.0 is available. You have 0.5.0.");
    expect(available.link).toEqual({
      label: "Download twr.deb",
      url: "https://example.com/twr.deb",
    });
    expect(
      resultSummary("desktop.check_updates", {
        kind: "mac",
        automatic: true,
        state: "downloaded",
        current: "0.5.0",
        latest: "0.6.0",
      })?.restart,
    ).toBe(true);
    expect(
      resultSummary("desktop.check_updates", {
        kind: "mac",
        automatic: true,
        state: "up-to-date",
        current: "0.5.0",
      })?.text,
    ).toBe("You have the latest version, twr 0.5.0.");
  });

  it("shows a memory note's text", () => {
    expect(
      resultSummary("memory.read", {
        name: "style",
        description: "House style",
        body: "Use **tabs**.",
      }),
    ).toEqual({ text: "House style", body: "Use **tabs**." });
  });

  it("falls back to data for a method without a formatter or a result it doesn't know", () => {
    expect(resultSummary("job.start", { job: "J" })).toBeUndefined();
    expect(resultSummary("thread.rename", "odd")).toBeUndefined();
    const html = renderToStaticMarkup(
      createElement(CommandResult, {
        method: "job.start",
        value: { job: "J" },
        values: {},
        onOpenThread() {},
        onOpenLink() {},
        onUpdateSettings() {},
        onError() {},
      }),
    );
    expect(html).toContain('class="record"');
    expect(html).not.toContain("<details");
  });

  it("keeps the raw result under a closed Details", () => {
    const html = renderToStaticMarkup(
      createElement(CommandResult, {
        method: "thread.stop",
        value: { thread_name: "api", stopping: true, dropped_prompts: 0 },
        values: {},
        onOpenThread() {},
        onOpenLink() {},
        onUpdateSettings() {},
        onError() {},
      }),
    );
    expect(html).toContain("Stopping api.");
    expect(html).toMatch(
      /<details class="command-result-details"><summary>Details<\/summary>/,
    );
  });

  it("writes byte counts for people", () => {
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1536)).toBe("1.5 KB");
    expect(formatBytes(50 * 1024 * 1024)).toBe("50 MB");
    expect(formatBytes(undefined)).toBe("—");
  });
});
