import { Markdown } from "./ui";

type Section = { name?: string; text: string };
const OPEN = '<thread name="';
const CLOSE = "</thread>";

/** Orchestrator tags are whole lines, never inline markup. Like the TUI, a new
 * opening tag ends the previous section, and an unfinished section runs to EOF.
 * Code examples keep their literal tags. */
export function threadSections(text: string, streaming = false): Section[] {
  if (!text.includes("<")) return [{ text }];
  const sections: Section[] = [];
  let name: string | undefined;
  let body: string[] = [];
  let fence: { character: string; length: number } | undefined;
  const flush = () => {
    const content = body.join("\n");
    if (name !== undefined || content.trim())
      sections.push({ name, text: content });
    body = [];
  };
  const lines = text.split("\n");
  for (const [index, line] of lines.entries()) {
    if (fence) {
      body.push(line);
      const end = /^ {0,3}(`+|~+)[ \t]*\r?$/.exec(line);
      if (end && end[1][0] === fence.character && end[1].length >= fence.length)
        fence = undefined;
      continue;
    }
    const start = /^ {0,3}(`{3,}|~{3,})(.*)$/.exec(line);
    if (start && (start[1][0] !== "`" || !start[2].includes("`"))) {
      fence = { character: start[1][0], length: start[1].length };
      body.push(line);
      continue;
    }
    // Four spaces or a tab is Markdown code, not a protocol delimiter.
    if (/^( {4}|\t)/.test(line)) {
      body.push(line);
      continue;
    }
    const tag = line.trim();
    const opened = /^<thread name="([^"\r\n]+)">$/.exec(tag)?.[1].trim();
    if (opened || tag === CLOSE) {
      flush();
      name = opened || undefined;
    } else if (
      streaming &&
      index === lines.length - 1 &&
      tag &&
      (CLOSE.startsWith(tag) ||
        OPEN.startsWith(tag) ||
        (tag.startsWith(OPEN) && !tag.includes(">")))
    ) {
      // Hold a trailing partial delimiter until the next streaming update.
      body.push("");
    } else {
      body.push(line);
    }
  }
  flush();
  return sections;
}

export function ThreadMessage({
  text,
  streaming,
}: {
  text: string;
  streaming: boolean;
}) {
  return (
    <div className="thread-sections">
      {threadSections(text, streaming).map((section, index) =>
        section.name === undefined ? (
          <Markdown key={index} text={section.text} />
        ) : (
          <section
            key={index}
            className="thread-message"
            aria-label={section.name}
          >
            <div className="thread-message-name">{section.name}</div>
            <Markdown text={section.text} />
          </section>
        ),
      )}
    </div>
  );
}
