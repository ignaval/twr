import { afterEach, describe, expect, it, vi } from "vitest";
import {
  LAST_HUB_KEY,
  autoConnectTarget,
  forgetLastHub,
  linkState,
  readLastHub,
  rememberLastHub,
} from "../mobile/src/connection";

describe("mobile header status", () => {
  it("is online while connected, reconnecting while retrying, offline once revoked", () => {
    expect(linkState(true, false)).toBe("online");
    expect(linkState(false, false, "Remote hub connection closed")).toBe(
      "reconnecting",
    );
    expect(
      linkState(false, false, "Gateway refused the connection (403)"),
    ).toBe("reconnecting");
    expect(linkState(false, true)).toBe("offline");
    expect(
      linkState(
        false,
        false,
        "AUTH_REVOKED: This device no longer has access to the hub.",
      ),
    ).toBe("offline");
  });
});

const hub = (id: string) => ({
  id,
  name: id,
  url: `https://${id}.invalid`,
  scopes: ["read"],
});
function memoryStorage() {
  const data = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => data.get(key) ?? null,
    setItem: (key: string, value: string) => data.set(key, value),
    removeItem: (key: string) => data.delete(key),
  });
  return data;
}
afterEach(() => {
  vi.unstubAllGlobals();
});

describe("auto-connecting to the last hub", () => {
  it("connects to the last hub only if it is still paired, once per launch", () => {
    const hubs = [hub("a"), hub("b")];
    expect(autoConnectTarget(hubs, "b", false)).toEqual(hub("b"));
    expect(autoConnectTarget(hubs, "b", true)).toBeUndefined();
    expect(autoConnectTarget(hubs, "gone", false)).toBeUndefined();
    expect(autoConnectTarget(hubs, undefined, false)).toBeUndefined();
    expect(autoConnectTarget([], "a", false)).toBeUndefined();
  });
  it("remembers the last hub and forgets it on disconnect, Forget and Revoke", () => {
    const data = memoryStorage();
    rememberLastHub("a");
    expect(data.get(LAST_HUB_KEY)).toBe("a");
    expect(readLastHub()).toBe("a");
    forgetLastHub("b");
    expect(readLastHub()).toBe("a");
    forgetLastHub("a");
    expect(readLastHub()).toBeUndefined();
    rememberLastHub("b");
    rememberLastHub(undefined);
    expect(data.has(LAST_HUB_KEY)).toBe(false);
  });
  it("works without storage", () => {
    vi.stubGlobal("localStorage", {
      getItem: () => {
        throw new Error("SecurityError");
      },
      setItem: () => {
        throw new Error("SecurityError");
      },
      removeItem: () => {
        throw new Error("SecurityError");
      },
    });
    expect(() => rememberLastHub("a")).not.toThrow();
    expect(readLastHub()).toBeUndefined();
    expect(() => forgetLastHub("a")).not.toThrow();
  });
});
