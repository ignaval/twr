import { useEffect, useId, useMemo, useRef, useState } from "react";
import { Markdown } from "./ui";

/** How often a still-streaming reply is parsed as Markdown again: each parse covers the
 * whole reply, so longer replies are parsed less often (250 ms, up to once a second). */
export function streamingInterval(length: number) {
  return Math.min(1000, 250 * Math.max(1, Math.ceil(length / 16384)));
}

/** The text to parse: while `streaming`, the latest text at most once per interval (it
 * still visibly streams); once final, the full text at once. */
function useStreamingText(text: string, streaming: boolean) {
  const [shown, setShown] = useState(text);
  const parsedAt = useRef(0);
  useEffect(() => {
    if (!streaming) return;
    const wait =
      streamingInterval(text.length) - (performance.now() - parsedAt.current);
    const show = () => {
      parsedAt.current = performance.now();
      setShown(text);
    };
    if (wait <= 0) {
      show();
      return;
    }
    const timer = setTimeout(show, wait);
    return () => clearTimeout(timer);
  }, [text, streaming]);
  return streaming ? shown : text;
}

/** Bound parser and DOM work while collapsed; expanding keeps the exact original text.
 * `expanded`/`onExpandedChange` let the conversation keep it open while the row is
 * scrolled out of the virtual list and back. */
export function MessageBody({
  text,
  mono = false,
  clamp = true,
  streaming = false,
  expanded: controlled,
  onExpandedChange,
}: {
  text: string;
  mono?: boolean;
  clamp?: boolean;
  streaming?: boolean;
  expanded?: boolean;
  onExpandedChange?: (expanded: boolean) => void;
}) {
  const [local, setLocal] = useState(false);
  const expanded = controlled ?? local;
  const setExpanded = (value: boolean) => {
    if (controlled === undefined) setLocal(value);
    onExpandedChange?.(value);
  };
  const id = useId();
  const parsed = useStreamingText(text, streaming && !mono);
  const { long, preview } = useMemo(() => {
    if (!clamp) return { long: false, preview: parsed };
    const head = parsed.slice(0, 1600);
    let lines = 1;
    let end = head.length;
    for (
      let index = head.indexOf("\n");
      index !== -1;
      index = head.indexOf("\n", index + 1)
    ) {
      lines++;
      if (lines === 13) end = Math.min(end, index);
    }
    // Keep a surrogate pair intact when the character cap intersects an emoji.
    if (end > 0 && /[\uD800-\uDBFF]/.test(parsed[end - 1])) end--;
    return {
      long: lines > 16 || parsed.length > 1600,
      preview: head.slice(0, end),
    };
  }, [parsed, clamp]);
  const collapsed = long && !expanded;
  const content = collapsed ? preview : parsed;
  return (
    <>
      <div className={`message-body ${collapsed ? "clamped" : ""}`} id={id}>
        {mono ? <pre>{content}</pre> : <Markdown text={content} />}
      </div>
      {long && (
        <button
          className="message-expand"
          aria-expanded={expanded}
          aria-controls={id}
          onClick={() => setExpanded(!expanded)}
        >
          {expanded ? "Show less" : "Show full message"}
        </button>
      )}
    </>
  );
}
