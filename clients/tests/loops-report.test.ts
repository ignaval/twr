import { describe, expect, it, vi } from "vitest";
import type { Json, Thread } from "../shared/types";
import { reportView, sizeWords, type LoopReport } from "../shared/loops";
import { FakeEvent, installFakeDOM, type FakeElement } from "./fake-dom";

/* Open report: the inspector's Loops section reads the loop's report.md through the hub
 * (`loop.report`) and shows its text in a dialog, so a phone or a remote desktop, which
 * cannot open the hub's files, reads it too. */

const report = (extra: Partial<LoopReport> = {}): LoopReport => ({
  thread: "t1",
  thread_name: "api",
  id: "l1",
  kind: "implement",
  running: false,
  outcome: "clean",
  path: "/hub/loops/l1/report.md",
  written: true,
  text: "# Implementation loop report\n\nClean after 3 rounds.",
  bytes: 2048,
  truncated: false,
  note: null,
  ...extra,
});

describe("the report's words", () => {
  it("titles the report, and says how the loop ended and its size", () => {
    const view = reportView(report());
    expect(view.title).toBe("Implementation loop report · api");
    expect(view.subtitle).toBe("clean · 2.0 KiB");
    expect(view.text).toContain("Clean after 3 rounds.");
    expect(view.note).toBeUndefined();
    expect(view.path).toBe("/hub/loops/l1/report.md");
  });

  it("keeps the hub's marker line of a report cut at 256 KiB", () => {
    const text =
      "# Report\n\nfirst lines\n\n[cut at 256 KiB: the whole report is at /hub/r.md]";
    const view = reportView(report({ text, truncated: true, bytes: 900_000 }));
    expect(view.text).toBe(text);
    expect(view.subtitle).toBe("clean · 878.9 KiB");
  });

  it("says why there is no text: the hub's note, else its own", () => {
    const running = reportView(
      report({
        running: true,
        outcome: null,
        written: false,
        text: null,
        bytes: null,
        note: "api's implementation loop is still running: its report is not written yet",
      }),
    );
    expect(running.text).toBeUndefined();
    expect(running.subtitle).toBe("running");
    expect(running.note).toMatch(/not written yet/);
    expect(
      reportView(report({ running: true, text: null, note: null })).note,
    ).toMatch(/still running/);
    expect(
      reportView(
        report({ outcome: "stopped", text: null, path: null, note: null }),
      ),
    ).toMatchObject({
      note: "This loop wrote no report.",
      path: undefined,
      subtitle: "stopped · 2.0 KiB",
    });
  });

  it("says sizes as attachments do", () => {
    expect(sizeWords(1)).toBe("1 byte");
    expect(sizeWords(812)).toBe("812 bytes");
    expect(sizeWords(12_600)).toBe("12.3 KiB");
    expect(sizeWords(3 * 1024 * 1024)).toBe("3.0 MiB");
  });
});

const worker: Thread = {
  id: "t1",
  name: "api",
  runtime: "claude-code",
  box_name: "local",
  mode: "auto",
  status: "done",
  accent: "#fff",
  is_orchestrator: false,
  last_loop: { id: "l1", kind: "implement", outcome: "clean", ended_turn: 4 },
  last_turn: { turn: 4 },
};

/** Mounts the inspector's Loops section on a device with only the `read` scope, its hub
 * answering `loop.get` and `loop.report` with `answer`. */
async function mount(answer: LoopReport | Error, detail: Json = {}) {
  vi.resetModules();
  const document = installFakeDOM();
  (globalThis as any).IS_REACT_ACT_ENVIRONMENT = true;
  const copied: string[] = [];
  (window as any).twr = {
    copyText: async (text: string) => void copied.push(text),
  };
  const calls: [string, Json][] = [];
  const rpc = async (method: string, params?: Json): Promise<Json> => {
    calls.push([method, params]);
    if (method === "loop.get")
      return {
        id: "l1",
        kind: "implement",
        running: false,
        folder: "/hub/loops/l1",
        report_path: "/hub/loops/l1/report.md",
        commit_checks: [],
        ...detail,
      };
    if (method === "loop.report") {
      if (answer instanceof Error) throw answer;
      return answer;
    }
    if (method === "loop.settings.get") throw new Error("not in this test");
    throw new Error(`unexpected ${method}`);
  };
  const { act, createElement } = await import("react");
  const { createRoot } = await import("react-dom/client");
  const { LoopContext, LoopsSection } = await import("../shared/LoopViews");
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container as unknown as Element);
  const settle = () =>
    act(async () => {
      for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 0));
    });
  await act(async () =>
    root.render(
      createElement(
        LoopContext.Provider,
        {
          value: {
            rpc,
            fail: () => {},
            access: { act: false, why: "This device has read-only access." },
            offerReview: true,
            startReview: async () => false,
            startImplement: async () => false,
            control: async () => false,
            openEditor: () => {},
            confirm: () => {},
            settingsVersion: 0,
            settingsChanged: () => {},
          },
        },
        createElement(LoopsSection, { thread: worker, connected: true }),
      ),
    ),
  );
  await settle();
  const find = (selector: string, text?: string) =>
    document
      .querySelectorAll(selector)
      .find((node) => text === undefined || node.textContent.includes(text)) as
      FakeElement | undefined;
  const click = async (node: FakeElement) => {
    await act(async () => {
      node.dispatchEvent(
        new FakeEvent("click", { bubbles: true, cancelable: true, button: 0 }),
      );
    });
    await settle();
  };
  return {
    calls,
    copied,
    find,
    click,
    unmount: () => act(async () => root.unmount()),
  };
}

describe("Open report", () => {
  it("shows the report's text in a dialog, read through the hub, with only the read scope", async () => {
    const m = await mount(report());
    const open = m.find("button", "Open report");
    expect(open).toBeDefined();
    expect(open!.getAttribute("disabled")).toBeNull();
    expect(open!.getAttribute("aria-haspopup")).toBe("dialog");
    await m.click(open!);
    expect(m.calls).toContainEqual(["loop.report", { id: "l1" }]);
    const dialog = m.find('[role="dialog"]');
    expect(dialog).toBeDefined();
    expect(dialog!.textContent).toContain("Implementation loop report · api");
    expect(
      m.find(".loop-report-body .markdown", "Clean after 3 rounds."),
    ).toBeDefined();
    // The path is secondary: small, in the footer, copyable.
    const path = m.find(".loop-report-footer code");
    expect(path?.textContent).toBe("/hub/loops/l1/report.md");
    await m.click(m.find('[aria-label="Copy report path"]')!);
    expect(m.copied).toEqual(["/hub/loops/l1/report.md"]);
    await m.click(m.find('[aria-label="Close dialog"]')!);
    expect(m.find('[role="dialog"]')).toBeUndefined();
    await m.unmount();
  });

  it("says why there is no text, and what the hub could not do", async () => {
    const m = await mount(
      report({
        running: true,
        text: null,
        written: false,
        note: "api's implementation loop is still running: its report is not written yet",
      }),
    );
    await m.click(m.find("button", "Open report")!);
    expect(m.find(".loop-report-body", "not written yet")).toBeDefined();
    expect(m.find(".loop-report-body .markdown")).toBeUndefined();
    await m.unmount();
    const failed = await mount(new Error("loop.report is not available"));
    await failed.click(failed.find("button", "Open report")!);
    expect(
      failed.find('.loop-report-body [role="alert"]', "not available"),
    ).toBeDefined();
    await failed.unmount();
  });
});

describe("the commit check's notes", () => {
  it("shows the last check's notes under the commits, quietly, as no problem", async () => {
    const note =
      "the thread wrote no twr-commit block (the check reads git, not the block)";
    const m = await mount(report(), {
      commit_checks: [
        { passed: false, problems: ["/w: dirty"], notes: ["an older note"] },
        {
          passed: true,
          commits: [{ repo: "/w", sha: "abc1234def" }],
          notes: [note],
        },
      ],
    });
    const notes = m.find('[aria-label="Commit check notes"]');
    expect(notes?.textContent).toBe(note);
    expect(notes?.querySelectorAll("li.muted")).toHaveLength(1);
    expect(m.find(".loop-problem")).toBeUndefined();
    await m.unmount();
  });
});
