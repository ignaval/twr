import { RateLimiter } from "./diagnostics";

/** What to do when the window's renderer process ends. */
export type RendererGoneAction = "ignore" | "reload" | "ask";

/**
 * A renderer that ends unexpectedly (out of memory, a GPU crash) leaves a blank
 * window, and a hidden background window would stop updating the tray. Reload it
 * on its own up to three times in five minutes; after that, ask, so a page that
 * keeps crashing does not reload forever.
 */
export class RendererRecovery {
  private readonly budget: RateLimiter;
  constructor(limit = 3, windowMs = 5 * 60_000, now: () => number = Date.now) {
    this.budget = new RateLimiter(limit, windowMs, now);
  }
  gone(reason: string, { quitting = false, requested = false } = {}) {
    if (reason === "clean-exit" || quitting) return "ignore" as const;
    // The user chose Reload for a hung window, which ends its renderer first.
    if (requested) return "reload" as const;
    return this.budget.allow() ? ("reload" as const) : ("ask" as const);
  }
}
