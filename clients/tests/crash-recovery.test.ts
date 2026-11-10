import { describe, expect, it } from "vitest";
import { RendererRecovery } from "../desktop/crash-recovery";

describe("renderer crash recovery", () => {
  it("reloads three times in five minutes, then asks", () => {
    let now = 0;
    const recovery = new RendererRecovery(undefined, undefined, () => now);
    expect(recovery.gone("oom")).toBe("reload");
    now += 60_000;
    expect(recovery.gone("crashed")).toBe("reload");
    now += 60_000;
    expect(recovery.gone("gpu-dead")).toBe("reload");
    now += 60_000;
    expect(recovery.gone("crashed")).toBe("ask");
    // The first reload falls out of the five-minute window.
    now = 5 * 60_000;
    expect(recovery.gone("crashed")).toBe("reload");
    expect(recovery.gone("crashed")).toBe("ask");
  });

  it("ignores a clean exit and the end of the app", () => {
    const recovery = new RendererRecovery(1);
    expect(recovery.gone("clean-exit")).toBe("ignore");
    expect(recovery.gone("killed", { quitting: true })).toBe("ignore");
    expect(recovery.gone("killed")).toBe("reload");
  });

  it("always honours a reload the user asked for, without spending the budget", () => {
    const recovery = new RendererRecovery(1);
    expect(recovery.gone("killed", { requested: true })).toBe("reload");
    expect(recovery.gone("killed", { requested: true })).toBe("reload");
    expect(recovery.gone("crashed")).toBe("reload");
    expect(recovery.gone("crashed")).toBe("ask");
  });
});
