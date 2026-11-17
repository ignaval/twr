import { afterEach, expect, it, vi } from "vitest";
import { FakeEvent, installFakeDOM, type FakeElement } from "./fake-dom";
import type { DesktopAPI, DesktopEvent, Json } from "../shared/types";

type Mounted = Awaited<ReturnType<typeof mount>>;
let mounted: Mounted | undefined;

async function mount(live = false, desktop = true) {
  const document = installFakeDOM();
  (globalThis as any).IS_REACT_ACT_ENVIRONMENT = true;
  vi.useFakeTimers();
  const { act, createElement } = await import("react");
  const { createRoot } = await import("react-dom/client");
  const { ReadAloudControl } = await import("../shared/ReadAloudControl");
  const listeners = new Set<(event: DesktopEvent) => void>();
  const speakers: Record<string, Json[]> = {
    work: [
      { device: "phone", live: !live, paused: false },
      // A local TUI can speak while the desktop's own process stands by.
      { device: "computer", name: "local TUI", live, paused: false },
    ],
    other: [{ device: "computer", live: !live, paused: false }],
  };
  let workspace = "work";
  let connected = true;
  let startDevice: string | undefined = "computer";
  const start = vi.fn(async () => ({
    device: startDevice,
    running: true,
    active: false,
    name: "desktop",
    playing: false,
    paused: false,
    queued: 0,
  }));
  const rpc = vi.fn(async (method: string, params?: Json): Promise<Json> => {
    if (method === "speech.status")
      return { speakers: structuredClone(speakers[workspace]) };
    if (method === "speech.control") {
      speakers[workspace].find((s) => s.device === params.speaker)!.live =
        params.value;
      return { speakers: 1 };
    }
    throw new Error(`Unexpected method ${method}`);
  });
  const host = {
    voiceSpeaker: desktop ? start : undefined,
    onEvent: (read: (event: DesktopEvent) => void) => {
      listeners.add(read);
      return () => listeners.delete(read);
    },
  } as unknown as DesktopAPI;
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container as unknown as Element);
  const flush = async () => {
    for (let i = 0; i < 15; i++) await Promise.resolve();
  };
  const render = async () => {
    await act(async () => {
      root.render(
        createElement(ReadAloudControl, { host, rpc, workspace, connected }),
      );
      await flush();
    });
  };
  const button = () => container.querySelector("button") as FakeElement;
  const click = async () => {
    await act(async () => {
      button().dispatchEvent(new FakeEvent("click", { bubbles: true }));
      await flush();
    });
  };
  const tick = async (ms = 1000) => {
    await act(async () => {
      await vi.advanceTimersByTimeAsync(ms);
      await flush();
    });
  };
  const emit = async (event: DesktopEvent) => {
    await act(async () => {
      listeners.forEach((read) => read(event));
      await flush();
    });
  };
  const close = async () => act(() => root.unmount());
  await render();
  return {
    act,
    button,
    click,
    tick,
    emit,
    rpc,
    start,
    speakers,
    render,
    close,
    switchWorkspace: async (next: string) => {
      workspace = next;
      await render();
    },
    connect: async (on: boolean) => {
      connected = on;
      await render();
    },
    identify: (device?: string) => {
      startDevice = device;
    },
  };
}

afterEach(async () => {
  await mounted?.close();
  mounted = undefined;
  vi.useRealTimers();
});

it.each([true, false])(
  "shows persisted live=%s without changing it or using the microphone",
  async (live) => {
    mounted = await mount(live);
    expect(mounted.button().getAttribute("aria-pressed")).toBe(String(live));
    expect(mounted.button().getAttribute("aria-label")).toContain(
      "orchestrator replies",
    );
    expect(mounted.button().getAttribute("title")).toContain(
      live ? "On" : "Off",
    );
    expect(mounted.button().classList.contains("on")).toBe(live);
    expect(mounted.button().classList.contains("no-drag")).toBe(true);
    expect(mounted.start).toHaveBeenCalledWith("start");
    expect(
      mounted.rpc.mock.calls.every(([method]) => method === "speech.status"),
    ).toBe(true);
  },
);

it("toggles only this computer in this workspace, including a standby desktop", async () => {
  mounted = await mount();
  await mounted.click();
  expect(mounted.rpc).toHaveBeenCalledWith("speech.control", {
    op: "live",
    value: true,
    speaker: "computer",
  });
  expect(mounted.button().getAttribute("aria-pressed")).toBe("true");
  await mounted.click();
  expect(mounted.rpc).toHaveBeenLastCalledWith("speech.status");
  expect(mounted.speakers.work[1].live).toBe(false);
  expect(mounted.speakers.work[0].live).toBe(true);
  expect(mounted.speakers.other[0].live).toBe(true);
  // Live-off already silences automatic playback. A broad stop would silence other workspaces.
  expect(
    mounted.rpc.mock.calls
      .filter(([m]) => m === "speech.control")
      .map(([, p]) => p.op),
  ).toEqual(["live", "live"]);
});

it("follows external controls, hands-free changes, pauses and speaker errors", async () => {
  mounted = await mount();
  mounted.speakers.work[1].live = true;
  mounted.speakers.work[1].paused = true;
  await mounted.tick();
  expect(mounted.button().getAttribute("aria-pressed")).toBe("true");
  expect(mounted.button().getAttribute("title")).toContain("playback paused");
  mounted.speakers.work[1].live = false;
  await mounted.emit({ type: "voice-speaker", speaker: await mounted.start() });
  expect(mounted.button().getAttribute("aria-pressed")).toBe("false");
  mounted.speakers.work[1].error = "Speech engine failed";
  await mounted.tick();
  expect(mounted.button().getAttribute("title")).toContain(
    "Speech engine failed",
  );
  await mounted.click();
  expect(mounted.button().getAttribute("aria-pressed")).toBe("true");
});

it("waits for acknowledgement and ignores rapid clicks", async () => {
  mounted = await mount();
  mounted.rpc.mockImplementationOnce(async () => ({ speakers: 1 }));
  await mounted.click();
  expect(mounted.button().getAttribute("aria-pressed")).toBe("false");
  expect(mounted.button().getAttribute("aria-busy")).toBe("true");
  await mounted.click();
  expect(
    mounted.rpc.mock.calls.filter(([m]) => m === "speech.control"),
  ).toHaveLength(1);
  mounted.speakers.work[1].live = true;
  await mounted.tick(250);
  expect(mounted.button().getAttribute("aria-pressed")).toBe("true");
  expect(mounted.button().getAttribute("aria-busy")).toBeNull();
});

it("reports rejected and unconfirmed controls without inventing a live setting", async () => {
  mounted = await mount();
  mounted.rpc.mockImplementationOnce(async () => ({ speakers: 0 }));
  await mounted.click();
  expect(mounted.button().getAttribute("title")).toContain("did not accept");
  expect(mounted.button().getAttribute("aria-pressed")).toBe("false");
  mounted.rpc.mockImplementationOnce(async () => ({ speakers: 1 }));
  await mounted.click();
  await mounted.tick(5000);
  expect(mounted.button().getAttribute("aria-pressed")).toBe("false");
  expect(mounted.button().getAttribute("title")).toContain("has not confirmed");
  expect(mounted.button().getAttribute("aria-busy")).toBeNull();
});

it("clears stale state on read failure, detach, workspace switch and reconnect", async () => {
  mounted = await mount(true);
  mounted.rpc.mockRejectedValueOnce(new Error("Connection lost"));
  await mounted.tick();
  expect(mounted.button().getAttribute("aria-pressed")).toBe("mixed");
  expect(mounted.button().getAttribute("title")).toContain("Connection lost");
  await mounted.tick();
  expect(mounted.button().getAttribute("aria-pressed")).toBe("true");
  await mounted.switchWorkspace("other");
  expect(mounted.button().getAttribute("aria-pressed")).toBe("false");
  await mounted.connect(false);
  expect(mounted.button().hasAttribute("disabled")).toBe(true);
  expect(mounted.button().getAttribute("aria-pressed")).toBe("mixed");
  mounted.speakers.other[0].live = true;
  await mounted.connect(true);
  expect(mounted.button().getAttribute("aria-pressed")).toBe("true");
  mounted.speakers.other = [];
  await mounted.tick();
  expect(mounted.button().getAttribute("aria-pressed")).toBe("mixed");
  expect(mounted.button().getAttribute("title")).toContain(
    "Click to reconnect",
  );
});

it("never broadcasts while waiting for the device identity, then recovers on a speaker event", async () => {
  mounted = await mount();
  await mounted.connect(false);
  mounted.identify(undefined);
  await mounted.connect(true);
  await mounted.click();
  expect(
    mounted.rpc.mock.calls.filter(([m]) => m === "speech.control"),
  ).toHaveLength(0);
  mounted.identify("computer");
  await mounted.emit({ type: "voice-speaker", speaker: await mounted.start() });
  expect(mounted.button().getAttribute("aria-pressed")).toBe("false");
  await mounted.click();
  expect(mounted.button().getAttribute("aria-pressed")).toBe("true");
});

it("cancels confirmation polling before it can affect a newly selected workspace", async () => {
  mounted = await mount();
  mounted.rpc.mockImplementationOnce(async () => ({ speakers: 1 }));
  await mounted.click();
  await mounted.switchWorkspace("other");
  mounted.rpc.mockClear();
  await mounted.tick(500);
  expect(mounted.rpc).not.toHaveBeenCalled();
  expect(mounted.button().getAttribute("aria-pressed")).toBe("true");
});

it("omits the control where the local desktop speaker bridge is unavailable", async () => {
  mounted = await mount(false, false);
  expect(mounted.button()).toBeNull();
  expect(mounted.start).not.toHaveBeenCalled();
  expect(mounted.rpc).not.toHaveBeenCalled();
});

it("reports speaker startup errors and retries without changing the saved setting", async () => {
  mounted = await mount(true);
  await mounted.connect(false);
  mounted.start.mockRejectedValueOnce(new Error("Speaker could not start"));
  await mounted.connect(true);
  expect(mounted.button().getAttribute("aria-pressed")).toBe("mixed");
  expect(mounted.button().getAttribute("title")).toContain(
    "Speaker could not start",
  );
  await mounted.click();
  expect(mounted.button().getAttribute("aria-pressed")).toBe("true");
  expect(
    mounted.rpc.mock.calls.filter(([m]) => m === "speech.control"),
  ).toHaveLength(0);
});

it("ignores a late startup result after changing workspace", async () => {
  mounted = await mount();
  await mounted.connect(false);
  const info = await mounted.start();
  let finish!: (value: typeof info) => void;
  mounted.start.mockImplementationOnce(
    () =>
      new Promise((resolve) => {
        finish = resolve;
      }),
  );
  await mounted.connect(true);
  await mounted.switchWorkspace("other");
  mounted.rpc.mockClear();
  await mounted.act(async () => {
    finish(info);
  });
  expect(mounted.rpc).not.toHaveBeenCalled();
  expect(mounted.button().getAttribute("aria-pressed")).toBe("true");
});
