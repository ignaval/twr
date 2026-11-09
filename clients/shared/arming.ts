/* An attention item's answers count only once its options have been on screen for
 * ARM_MS, as in the TUI: a second click of a double click, a key pressed as a card slid
 * into place, or a click just as the item was raised again was meant for something else.
 * Pure state, so it is tested without a DOM; the card feeds it visibility changes. */
export const ARM_MS = 600;

export interface Arming {
  /** When the options last became visible (monotonic ms); undefined while hidden. */
  since?: number;
}

/** The options are on screen: keep the earlier time when they already were. */
export function shown(state: Arming, now: number): Arming {
  return state.since === undefined ? { since: now } : state;
}

/** The options left the screen (scrolled away, window hidden): arm again from zero. */
export function hidden(): Arming {
  return {};
}

export function armed(state: Arming, now: number): boolean {
  return state.since !== undefined && now - state.since >= ARM_MS;
}

/** Milliseconds until the options arm; undefined while they are not on screen. */
export function armsIn(state: Arming, now: number): number | undefined {
  if (state.since === undefined) return undefined;
  return Math.max(0, ARM_MS - (now - state.since));
}

/** Whether a click or key may answer now. `sent` stays true once an answer succeeded,
 * until the card goes away, so a second click never reaches a stale revision. */
export function mayAnswer({
  arming,
  now,
  busy,
  sent,
  readOnly,
}: {
  arming: Arming;
  now: number;
  busy: boolean;
  sent: boolean;
  readOnly: boolean;
}): boolean {
  return !readOnly && !busy && !sent && armed(arming, now);
}
