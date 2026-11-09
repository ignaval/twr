/* Which links open in one click and which ask first. A link's visible text is chosen by
 * whoever wrote it (an agent's reply, a program writing OSC 8 hyperlinks to a terminal),
 * so it must not be able to disguise where the link goes. Pure, so it is unit tested. */

/** The schemes the hosts open externally (desktop/validation.ts, remote.ts). */
const external = new Set(["https:", "http:", "mailto:"]);

/** The URL a link opens, or undefined for one the app doesn't open (a relative path,
 * `file:`, an anchor, any other scheme): those show as text that can be copied. */
export function externalLink(href: string | undefined): URL | undefined {
  if (!href) return undefined;
  try {
    const url = new URL(href);
    return external.has(url.protocol) ? url : undefined;
  } catch {
    return undefined;
  }
}

const normalHost = (host: string) =>
  host
    .toLowerCase()
    .replace(/\.$/, "")
    .replace(/^www\./, "");

/** The host a link's visible text names, when the text is itself a URL or a bare host
 * ("example.com/docs", "https://example.com"); undefined for ordinary words. */
export function textHost(text: string): string | undefined {
  const value = text.trim().replace(/^<(.*)>$/, "$1");
  if (!value || /\s/.test(value)) return undefined;
  try {
    if (/^[a-z][a-z0-9+.-]*:\/\//i.test(value))
      return normalHost(new URL(value).hostname) || undefined;
    const bare =
      /^((?:[\p{L}\p{N}-]+\.)+([\p{L}]{2,}|xn--[a-z0-9-]+))\.?((?::\d+)?(?:[/?#]\S*)?)$/iu.exec(
        value,
      );
    const ip = /^\d{1,3}(?:\.\d{1,3}){3}(?::\d+)?(?:[/?#]\S*)?$/.test(value);
    // A file name ("App.tsx", "README.md") looks like a host too: count it as one
    // only with www., a port or path, or a common top-level domain.
    if (
      ip ||
      (bare &&
        (/^www\./i.test(value) ||
          bare[3] !== "" ||
          commonDomains.has(bare[2].toLowerCase()) ||
          (bare[2].length === 2 && !fileExtensions.has(bare[2].toLowerCase()))))
    )
      return normalHost(new URL("https://" + value).hostname);
  } catch {}
  return undefined;
}
const commonDomains = new Set(
  (
    "com net org io dev app ai co edu gov mil info biz me tv xyz site online tech " +
    "cloud page link live news shop store blog email support security bank pay"
  ).split(" "),
);
const fileExtensions = new Set(
  "md py rs sh js ts go rb pl cs cc hh mk so db gz xz ps ml hs ex kt cl el".split(
    " ",
  ),
);

const emailText = (text: string) => {
  const value = text.trim().replace(/^mailto:/i, "");
  return /^[^\s@]+@[^\s@]+$/.test(value) ? value.toLowerCase() : undefined;
};

/** Why opening this link should be confirmed first, or undefined to open it at once.
 * `terminal` is an OSC 8 hyperlink: its text and its address are set separately. */
export function linkConfirmation({
  href,
  text,
  terminal = false,
}: {
  href: string;
  text: string;
  terminal?: boolean;
}): string | undefined {
  const url = externalLink(href);
  if (!url) return undefined;
  const shown = text.trim();
  if (terminal && shown !== href && shown !== url.href)
    return "The terminal shows different text for this link than the address it opens.";
  if (url.protocol === "mailto:") {
    const address = decodeURIComponent(url.pathname).toLowerCase();
    const named = emailText(shown);
    if (named && named !== address)
      return `The link's text names ${named}, but it writes to ${address}.`;
    return undefined;
  }
  const named = textHost(shown);
  const host = normalHost(url.hostname);
  if (named && named !== host)
    return `The link's text names ${named}, but it opens ${host}.`;
  if (url.protocol === "http:")
    return "This link uses plain http: what is sent and received isn't encrypted.";
  return undefined;
}
