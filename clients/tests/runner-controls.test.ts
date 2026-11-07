import { expect, it, vi } from "vitest";
import { routeRunnerControls } from "../desktop/runner-controls";

it("hosts and cancels on this desktop while pairing and access stay on the viewed hub", async () => {
  const local = vi.fn(async () => "local");
  const remote = vi.fn(async () => "remote");
  const rpc = routeRunnerControls(local, remote);
  for (const method of [
    "desktop.runner.use",
    "desktop.runner.cancel",
    "desktop.runner.status",
  ]) {
    const params = { session: "this-desktop" };
    expect(await rpc(method, params)).toBe("local");
    expect(local).toHaveBeenLastCalledWith(method, params);
  }
  expect(remote).not.toHaveBeenCalled();
  for (const method of [
    "runner.connect",
    "runner.manage",
    "desktop.unrecognized_private_method",
  ]) {
    const params = { name: "runner-on-viewed-hub" };
    expect(await rpc(method, params)).toBe("remote");
    expect(remote).toHaveBeenLastCalledWith(method, params);
  }
  expect(local).toHaveBeenCalledTimes(3);
});

it("retires an exact local code after the old hub view is closed, without reopening other controls", async () => {
  const local = vi.fn(async () => "local");
  const remote = vi.fn(async () => "remote");
  const rpc = routeRunnerControls(local, remote, () => false);
  expect(await rpc("desktop.runner.cancel", { session: "old-window" })).toBe(
    "local",
  );
  for (const method of [
    "desktop.runner.use",
    "desktop.runner.status",
    "desktop.runner.cancel",
    "runner.manage",
  ]) {
    await expect(rpc(method)).rejects.toThrow("closed");
  }
  expect(local).toHaveBeenCalledTimes(1);
  expect(remote).not.toHaveBeenCalled();
});
