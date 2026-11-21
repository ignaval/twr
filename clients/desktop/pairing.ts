import { parseInvitation } from "../shared/connections";

/** The Pair form's fields (Connections.tsx › Connect to another computer). */
export interface PairFields {
  hubName: string;
  url: string;
  code: string;
  name: string;
}

/** A name for the hub an invitation comes from: its host's first label ("workstation" for
 * `https://workstation.example:47321`), an IP address whole. */
export function hubNameFor(url: string): string {
  const host = new URL(url).hostname.replace(/^\[|\]$/g, "");
  if (/^[\d.]+$/.test(host) || host.includes(":")) return host;
  return host.split(".")[0] || host;
}

/** The fields a pasted invitation fills: its address and code, and a hub name when the
 * form has none yet. Undefined for text that isn't an invitation at all (typing in the
 * box); an error for one that is but is wrong. */
export function fieldsFromInvitation(
  text: string,
  current: PairFields,
): PairFields | undefined {
  const raw = text.trim();
  if (!raw.startsWith("{")) return undefined;
  const invitation = parseInvitation(raw);
  return {
    ...current,
    url: invitation.url,
    code: invitation.code,
    hubName: current.hubName.trim() || hubNameFor(invitation.url),
  };
}

/** What the Pair form still needs, in words, or nothing when it can pair. */
export function missingPairFields(fields: PairFields): string | undefined {
  const missing = [
    !fields.url.trim() && "the hub's address",
    !fields.code.trim() && "its one-use code",
    !fields.hubName.trim() && "a hub name",
    !fields.name.trim() && "this device's name",
  ].filter(Boolean) as string[];
  if (!missing.length) return undefined;
  return `Paste an invitation, or enter ${
    missing.length === 1
      ? missing[0]
      : missing.slice(0, -1).join(", ") + " and " + missing.at(-1)
  }.`;
}
