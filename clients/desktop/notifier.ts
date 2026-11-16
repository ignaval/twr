import { Notification } from "electron";
import type { AttentionSummary } from "../shared/types";
import type { DiagnosticLog } from "./diagnostics";
import {
  AttentionWatch,
  Burst,
  LoopWatch,
  answered,
  isLoopNotice,
  loopNotices,
  notices,
  stillWaiting,
  type Notice,
  type NoticeTarget,
} from "./notify-state";

/** A posted notification, as the notifier needs it: an OS one, or a test stand-in. */
export interface Posted {
  close(): void;
}
export type Post = (
  notice: Notice,
  handlers: { click(): void; failed(error: string): void },
) => Posted;

export interface NotifierHost {
  /** The setting (Settings › Application). */
  enabled(): boolean;
  /** The window shows and has focus: nothing is news to the user. */
  attended(): boolean;
  /** Shows and focuses the window and opens a notification's target. */
  open(target: NoticeTarget): void;
  log: DiagnosticLog;
}

/** Posts the system's own notifications, which follow its Do Not Disturb and Focus settings. */
export const systemPost: Post = (notice, handlers) => {
  const notification = new Notification({
    title: notice.title,
    body: notice.body,
  });
  notification.on("click", handlers.click);
  notification.on("failed", (_event, error) => handlers.failed(error));
  notification.show();
  return notification;
};

/**
 * OS notifications for new permissions, questions and the hub's confirm and delete cards in
 * any workspace while the window is not in front (desktop/notify-state.ts decides which), and
 * for a loop that ends, once, with how it ended. A click shows the window at the request (or
 * the loop's thread); it never answers it.
 */
export class Notifier {
  private watch = new AttentionWatch();
  private loops = new LoopWatch();
  private burst = new Burst();
  private timer?: ReturnType<typeof setTimeout>;
  private latest?: AttentionSummary;
  /** Posted notifications, held until they are closed so their click still works. */
  private posted = new Map<Posted, string[]>();
  private failures = 0;
  private toldUnsupported = false;
  private toldFirst = false;
  private readonly post: Post;
  readonly supported: boolean;
  private readonly now: () => number;
  constructor(
    private readonly host: NotifierHost,
    options: { post?: Post; supported?: boolean; now?: () => number } = {},
  ) {
    this.post = options.post || systemPost;
    this.supported = options.supported ?? Notification.isSupported();
    this.now = options.now || Date.now;
  }

  update(summary: AttentionSummary) {
    this.latest = summary;
    const news = this.watch.observe(summary, this.host.attended());
    if (news.length && this.active) {
      this.burst.add(news, this.now());
      this.schedule();
    }
    // A loop's end is no request: it goes out at once, and no answer takes it back.
    const ended = this.loops.observe(summary, this.host.attended());
    if (ended.length && this.active)
      for (const notice of loopNotices(ended)) this.show(notice);
    for (const [notification, keys] of this.posted)
      if (!isLoopNotice(keys) && answered(keys, summary))
        this.remove(notification);
  }

  /** The window came to the front: what it shows needs no notification any more. */
  attend() {
    this.burst.clear();
    clearTimeout(this.timer);
    this.timer = undefined;
    for (const notification of [...this.posted.keys()])
      this.remove(notification);
  }

  /** The setting was turned off, or the window is gone. */
  clear() {
    this.attend();
  }

  private get active() {
    if (!this.host.enabled()) return false;
    if (!this.supported) {
      if (!this.toldUnsupported) {
        this.toldUnsupported = true;
        this.host.log.info(
          "notify",
          "This system does not support notifications; none are posted",
        );
      }
      return false;
    }
    return true;
  }

  private schedule() {
    if (this.timer) return;
    const wait = this.burst.dueIn(this.now());
    if (wait === undefined) return;
    this.timer = setTimeout(() => {
      this.timer = undefined;
      this.flush();
    }, wait);
    this.timer.unref?.();
  }

  private flush() {
    const items = this.burst.take();
    // Answered while held, or the user came back to the window: nothing to say.
    if (!this.latest || this.host.attended() || !this.active) return;
    for (const notice of notices(stillWaiting(items, this.latest)))
      this.show(notice);
  }

  private show(notice: Notice) {
    try {
      let notification: Posted | undefined = undefined;
      notification = this.post(notice, {
        click: () => {
          if (notification) this.remove(notification);
          this.host.open(notice.target);
        },
        failed: (error) => {
          if (notification) this.posted.delete(notification);
          this.failed(error);
        },
      });
      this.posted.set(notification, notice.keys);
      if (!this.toldFirst) {
        this.toldFirst = true;
        this.host.log.info(
          "notify",
          "Posted the first notification. macOS asks whether twr may notify; the answer is in System Settings › Notifications.",
        );
      }
    } catch (error) {
      this.failed(error);
    }
  }

  private failed(error: unknown) {
    // A build the system won't let notify fails every time: say so a few times only.
    if (++this.failures <= 3)
      this.host.log.warn("notify", "Could not post a notification", error);
  }

  private remove(notification: Posted) {
    if (!this.posted.delete(notification)) return;
    try {
      notification.close();
    } catch {}
  }
}
