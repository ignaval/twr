import { afterEach, describe, expect, it, vi } from "vitest";
import type { Json } from "../shared/types";
import { installFakeDOM } from "./fake-dom";

/* The Usage page (shared/Managers.tsx, section `usage`), mounted on the stand-in DOM with
 * the hub answering by a mock `rpc`: its clock goes on from when the hub answered, also
 * while reading it again fails. */

type Rpc = (method: string, params?: Json) => Promise<Json>;

const t0 = Date.parse("2026-11-20T12:00:00Z");
const at = (secs: number) => new Date(t0 + secs * 1000).toISOString();

afterEach(() => {
  vi.restoreAllMocks();
});

describe("the Usage page", () => {
  it("counts down from the hub's answer, and drops a window that reset while refresh fails", async () => {
    let clock = t0;
    vi.spyOn(Date, "now").mockImplementation(() => clock);
    let online = true;
    const rpc: Rpc = async (method) => {
      if (method !== "usage.get") throw new Error(`unexpected ${method}`);
      if (!online) throw new Error("Not connected");
      return {
        now: at(0),
        runtimes: [
          {
            runtime: "claude-code",
            label: "Claude",
            age_secs: 0,
            windows: [
              {
                window: "five_hour",
                used_pct: 40,
                minutes: 300,
                resets_at: at(600),
              },
              { window: "seven_day", used_pct: 10, resets_at: at(86_400) },
            ],
          },
        ],
      };
    };
    vi.resetModules();
    const document = installFakeDOM();
    (globalThis as any).IS_REACT_ACT_ENVIRONMENT = true;
    const { act, createElement } = await import("react");
    const { createRoot } = await import("react-dom/client");
    const { Manager } = await import("../shared/Managers");
    const container = document.createElement("div");
    document.body.appendChild(container);
    const root = createRoot(container as unknown as Element);
    const settle = () =>
      act(async () => {
        for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 0));
      });
    const render = (refreshKey: number) =>
      act(async () =>
        root.render(
          createElement(Manager, {
            section: "usage",
            workspace: "default",
            refreshKey,
            rpc,
            action: () => {},
            cliAction: () => {},
            fail: () => {},
            canAdmin: true,
            connected: true,
          } as Json),
        ),
      );
    await render(0);
    await settle();
    const text = () => document.body.textContent;
    expect(text()).toContain("5-hour");
    expect(text()).toContain("resets in 10m");

    // The hub goes away; four minutes on, a read fails: the countdown still moves.
    online = false;
    clock = t0 + 4 * 60_000;
    await render(1);
    await settle();
    expect(text()).toContain("resets in 6m");
    expect(text()).toContain("Not connected");

    // Past the 5-hour window's reset: it goes, the weekly one stays.
    clock = t0 + 11 * 60_000;
    await render(2);
    await settle();
    expect(text()).not.toContain("5-hour");
    expect(text()).toContain("Weekly");
    await act(async () => root.unmount());
  });
});
