/**
 * What the app saves on this device after a delay (the drafts, 350 ms after typing stops)
 * registers here, and whoever sees the app go away asks for it now: the desktop window
 * before it closes or the app quits (`twr:app-save-now`, main.ts), the phone app as it goes
 * to the background (mobile/src/main.tsx), and any page as it is unloaded (`pagehide`).
 */
const savers = new Set<() => Promise<unknown>>();

/** Adds `save`, which `saveNow` runs; the returned function removes it. */
export function onSaveNow(save: () => Promise<unknown>): () => void {
  savers.add(save);
  return () => {
    savers.delete(save);
  };
}

/** Runs every saver at once and resolves once all of them are done, failed or not. */
export async function saveNow(): Promise<void> {
  await Promise.allSettled([...savers].map((save) => save()));
}
