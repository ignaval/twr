/**
 * The window's page saves its drafts on this device a moment after they change
 * (`shared/save-now.ts`). Before the window closes, or the app quits, the main process asks
 * it to save them now (`twr:app-save-now`) and holds the close until it answers
 * (`twr:app-saved`) or `timeout` ms pass; then the close goes ahead.
 */
export class SaveBeforeClose {
  private next = 0;
  private replies = new Map<number, () => void>();
  private saving?: Promise<void>;
  /** The page has just saved: the next close goes ahead without asking again. */
  private ready = false;

  constructor(
    /** Asks the page to save; false when there is no page to ask (it crashed, is gone). */
    private ask: (id: number) => boolean,
    private timeout = 2000,
  ) {}

  /**
   * Called from the window's `close` event: true lets this close go ahead. Otherwise the
   * caller prevents it; once the page has saved, `again` closes it (or quits) once more.
   */
  close(again: () => void): boolean {
    if (this.ready) {
      this.ready = false;
      return true;
    }
    this.saving ??= this.save().then(() => {
      this.saving = undefined;
      this.ready = true;
      again();
    });
    return false;
  }

  /** Saves before something that closes the window itself (an update's restart): the
   * close it makes goes ahead. */
  async beforeClosing() {
    await this.save();
    this.ready = true;
  }

  /** The page's answer to request `id`. */
  saved(id: number) {
    this.replies.get(id)?.();
  }

  /** The close did not happen (or a new window opened): the next one asks again. */
  reset() {
    this.ready = false;
  }

  private save() {
    const id = ++this.next;
    return new Promise<void>((resolve) => {
      const done = () => {
        clearTimeout(timer);
        this.replies.delete(id);
        resolve();
      };
      const timer = setTimeout(done, this.timeout);
      this.replies.set(id, done);
      let asked = false;
      try {
        asked = this.ask(id);
      } catch {}
      if (!asked) done();
    });
  }
}
