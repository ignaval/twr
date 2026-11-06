import { beforeEach, describe, expect, it, vi } from "vitest";
const sdk = vi.hoisted(() => ({
  listeners: new Map<string, Set<(value: any) => void>>(),
  register: vi.fn(),
  permission: vi.fn(),
  prompt: vi.fn(),
  channel: vi.fn(async () => {}),
}));
vi.mock("@capacitor/core", () => ({
  Capacitor: { getPlatform: () => "android" },
}));
vi.mock("@capacitor/push-notifications", () => ({
  PushNotifications: {
    addListener: async (name: string, callback: (value: any) => void) => {
      if (!sdk.listeners.has(name)) sdk.listeners.set(name, new Set());
      sdk.listeners.get(name)!.add(callback);
      return {
        remove: async () => {
          sdk.listeners.get(name)!.delete(callback);
        },
      };
    },
    checkPermissions: sdk.permission,
    requestPermissions: sdk.prompt,
    register: sdk.register,
    createChannel: sdk.channel,
  },
}));
vi.mock("@capacitor/local-notifications", () => ({
  LocalNotifications: { addListener: async () => ({ remove: async () => {} }) },
}));
import {
  enablePush,
  disablePush,
  refreshPush,
  pushEnabled,
  notificationListeners,
  pushPreferenceKey,
} from "../mobile/src/notifications";
import type { RemoteHost } from "../mobile/src/transport";
const emit = (value: string) => {
  for (const callback of sdk.listeners.get("registration") || [])
    callback({ value });
};
const host = () =>
  ({
    request: vi.fn(async (method: string) =>
      method === "push-status" ? { fcm: true } : {},
    ),
  }) as unknown as RemoteHost;
beforeEach(() => {
  vi.clearAllMocks();
  sdk.listeners.clear();
  const storage = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => storage.get(key) || null,
    setItem: (key: string, value: string) => storage.set(key, value),
    removeItem: (key: string) => storage.delete(key),
  });
  sdk.permission.mockResolvedValue({ receive: "granted" });
  sdk.prompt.mockResolvedValue({ receive: "granted" });
  sdk.register.mockImplementation(async () => emit("initial-token"));
});
describe("push token lifecycle", () => {
  it("disables push after an in-flight registration and leaves opt-in cleared", async () => {
    const remote = host();
    let finish: (() => void) | undefined;
    vi.mocked(remote.request).mockImplementation(async (method) => {
      if (method === "push-status") return { fcm: true };
      if (method === "push-register")
        await new Promise<void>((resolve) => {
          finish = resolve;
        });
      return {};
    });
    const enabling = enablePush(remote, "hub-a");
    await vi.waitFor(() => expect(finish).toBeDefined());
    const disabling = disablePush(remote, "hub-a");
    finish!();
    await Promise.all([enabling, disabling]);
    expect(pushEnabled("hub-a")).toBe(false);
    expect(remote.request).toHaveBeenLastCalledWith(
      "push-unregister",
      {},
      10000,
    );
    await refreshPush(remote, "hub-a");
    expect(sdk.register).toHaveBeenCalledTimes(1);
  });
  it("restores registration only for opted-in hubs and never prompts during passive refresh", async () => {
    const remote = host();
    await refreshPush(remote, "hub-a");
    expect(sdk.register).not.toHaveBeenCalled();
    localStorage.setItem(pushPreferenceKey("hub-a"), "true");
    sdk.permission.mockResolvedValue({ receive: "prompt" });
    await refreshPush(remote, "hub-a");
    expect(sdk.prompt).not.toHaveBeenCalled();
    expect(sdk.register).not.toHaveBeenCalled();
    sdk.permission.mockResolvedValue({ receive: "granted" });
    await refreshPush(remote, "hub-a");
    expect(remote.request).toHaveBeenCalledWith("push-register", {
      platform: "android",
      token: "initial-token",
    });
  });
  it("refreshes rotated tokens for the currently opted-in hub after initial registration finishes", async () => {
    const first = host(),
      second = host();
    let current: RemoteHost | undefined = first;
    const stop = await notificationListeners(
      () => {},
      () => current,
    );
    await enablePush(first, "hub-a");
    expect(pushEnabled("hub-a")).toBe(true);
    emit("rotated-token");
    await Promise.resolve();
    expect(first.request).toHaveBeenCalledWith("push-register", {
      platform: "android",
      token: "rotated-token",
    });
    current = second;
    emit("next-hub-token");
    await Promise.resolve();
    expect(second.request).toHaveBeenCalledWith("push-register", {
      platform: "android",
      token: "next-hub-token",
    });
    expect(first.request).not.toHaveBeenCalledWith("push-register", {
      platform: "android",
      token: "next-hub-token",
    });
    current = undefined;
    emit("opted-out-token");
    await Promise.resolve();
    expect(second.request).not.toHaveBeenCalledWith("push-register", {
      platform: "android",
      token: "opted-out-token",
    });
    stop();
  });
  it("coalesces concurrent registration and preserves an explicit permission request", async () => {
    const remote = host();
    await Promise.all([
      enablePush(remote, "hub-a"),
      enablePush(remote, "hub-a"),
    ]);
    expect(sdk.register).toHaveBeenCalledTimes(1);
    sdk.permission.mockResolvedValue({ receive: "prompt" });
    await Promise.all([
      refreshPush(remote, "hub-a"),
      enablePush(remote, "hub-a"),
    ]);
    expect(sdk.prompt).toHaveBeenCalledTimes(1);
    expect(sdk.register).toHaveBeenCalledTimes(2);
  });
});
