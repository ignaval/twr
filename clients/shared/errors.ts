export function errorMessage(error: unknown) {
  return (error instanceof Error ? error.message : String(error))
    .replace(/^Error invoking remote method '[^']+':\s*/i, "")
    .replace(/^Error:\s*/i, "")
    .replace(/REQUEST_REJECTED: /g, "");
}

/** A connection error as the status strip says it: the hub's socket path and the
 * reconnect loop's own words stay out of sight (`detail`, a tooltip), "Connecting to the
 * hub…" said instead. Any other error as it is. */
export function connectionWords(error: string): {
  text: string;
  detail?: string;
} {
  if (
    /^connecting to hub at /i.test(error) ||
    /^Hub connection closed; reconnecting/i.test(error)
  )
    return { text: "Connecting to the hub…", detail: error };
  return { text: error };
}
