import { isRevocationError } from "../../shared/remote";

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
