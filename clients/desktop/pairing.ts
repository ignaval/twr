import { parseInvitation } from "../shared/connections";
import { errorMessage } from "../shared/errors";

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

/** Most characters of a one-use code, a hub name and a device name (what
 * `twrRemote.pair` takes). */
export const PAIR_LIMITS = { codeMin: 4, code: 128, name: 80 };

/** What is wrong with details that are all there, in words (the limits `twrRemote.pair`
 * checks), or nothing when they can be sent. */
export function pairFieldProblem(fields: {
  url: string;
  code: string;
  hubName: string;
  name: string;
}): string | undefined {
  const code = fields.code.trim();
  const again = " Copy it again from the invitation on the other computer.";
  if (code.length < PAIR_LIMITS.codeMin)
    return "The one-use code is too short." + again;
  if (code.length > PAIR_LIMITS.code)
    return "The one-use code is too long." + again;
  if (fields.hubName.trim().length > PAIR_LIMITS.name)
    return `The hub name is too long. Use at most ${PAIR_LIMITS.name} characters.`;
  if (fields.name.trim().length > PAIR_LIMITS.name)
    return `This device's name is too long. Use at most ${PAIR_LIMITS.name} characters.`;
  let url: URL | undefined;
  try {
    url = new URL(fields.url.trim());
  } catch {
    url = undefined;
  }
  if (!url || !/^https?:$/.test(url.protocol))
    return "The hub URL isn't a web address. Enter it as the invitation shows it, such as https://workstation.example:47321.";
  return undefined;
}

/** The fields an invitation filled in (`filled`), emptied again: its text was cleared or
 * changed into something that isn't one. A field changed by hand since is kept. */
export function withoutInvitation(
  fields: PairFields,
  filled: Partial<PairFields> | undefined,
): PairFields {
  if (!filled) return fields;
  const next = { ...fields };
  for (const key of ["url", "code", "hubName"] as const)
    if (filled[key] !== undefined && next[key] === filled[key]) next[key] = "";
  return next;
}

/** What **Pair and connect** sends. With text in Paste invitation, what that text says now:
 * text that isn't a valid invitation is refused, whatever an earlier one filled in. Without,
 * the details entered by hand. `manual`: the error names a detail to enter by hand. */
export function pairRequest(
  invitationText: string,
  fields: PairFields,
): { fields: PairFields } | { error: string; manual: boolean } {
  const pasted = !!invitationText.trim();
  let request = fields;
  if (pasted) {
    const again =
      " Copy it again on the other computer, or clear Paste invitation to enter the details manually.";
    try {
      const filled = fieldsFromInvitation(invitationText, fields);
      if (!filled)
        return {
          error: "The pasted text isn't a twr invitation." + again,
          manual: false,
        };
      request = filled;
    } catch (error) {
      return {
        error: `The pasted invitation can't be used: ${errorMessage(error).replace(/\.$/, "")}.${again}`,
        manual: false,
      };
    }
  }
  const missing = missingPairFields(request) ?? pairFieldProblem(request);
  return missing ? { error: missing, manual: pasted } : { fields: request };
}
