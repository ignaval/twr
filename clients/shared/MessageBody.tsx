import { useMemo, useState } from "react";
import { Markdown } from "./ui";

/** Bound parser and DOM work while collapsed; expanding keeps the exact original text. */
export function MessageBody({
  text,
  mono = false,
  clamp = true,
}: {
  text: string;
  mono?: boolean;
  clamp?: boolean;
}) {
  const [expanded, setExpanded] = useState(false);
  const { long, preview } = useMemo(() => {
    if (!clamp) return { long: false, preview: text };
    const head = text.slice(0, 1600);
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
    if (end > 0 && /[\uD800-\uDBFF]/.test(text[end - 1])) end--;
    return {
      long: lines > 16 || text.length > 1600,
      preview: head.slice(0, end),
    };
  }, [text, clamp]);
  const collapsed = long && !expanded;
  const content = collapsed ? preview : text;
  return (
    <>
      <div className={`message-body ${collapsed ? "clamped" : ""}`}>
        {mono ? <pre>{content}</pre> : <Markdown text={content} />}
      </div>
      {long && (
        <button
          className="message-expand"
          aria-expanded={expanded}
          onClick={() => setExpanded((v) => !v)}
        >
          {expanded ? "Show less" : "Show full message"}
        </button>
      )}
    </>
  );
}
