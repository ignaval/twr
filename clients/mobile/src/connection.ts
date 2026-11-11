import { isRevocationError, type Hub } from "../../shared/remote";

/** What the header's status dot shows for the active hub. */
export type LinkState = "online" | "reconnecting" | "offline";

/**
 * The status dot after a `connection` event: online while the hub is reachable,
 * reconnecting while the app retries, offline once it stopped (a revoked device).
 */
export function linkState(
  connected: boolean,
  revoked: boolean,
  error?: string,
): LinkState {
  if (connected) return "online";
  if (revoked || isRevocationError(error ?? "")) return "offline";
  return "reconnecting";
}

/** The hub this phone last connected to: a per-device convenience, no credential. */
export const LAST_HUB_KEY = "twr.mobile.lastHub";

export function readLastHub(): string | undefined {
  try {
    return localStorage.getItem(LAST_HUB_KEY) || undefined;
  } catch {
    return undefined;
  }
}
/** Remembers `id`, or forgets the last hub when it is undefined. */
export function rememberLastHub(id: string | undefined) {
  try {
    if (id) localStorage.setItem(LAST_HUB_KEY, id);
    else localStorage.removeItem(LAST_HUB_KEY);
  } catch {
    /* Storage can be unavailable; the hub list still works. */
  }
}
/** Forgets the last hub if it is `id` (that hub was forgotten or revoked). */
export function forgetLastHub(id: string) {
  if (readLastHub() === id) rememberLastHub(undefined);
}

/**
 * The hub to connect to on a cold launch: the last one connected, if it is
 * still paired. Tried once per launch: a failed attempt leaves the hub list.
 */
export function autoConnectTarget(
  hubs: Hub[],
  lastHub: string | undefined,
  alreadyTried: boolean,
): Hub | undefined {
  if (alreadyTried || !lastHub) return undefined;
  return hubs.find((hub) => hub.id === lastHub);
}
