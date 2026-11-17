import type { Blockquote, Root, RootContent } from "mdast";
import type { Extension as FromMarkdownExtension } from "mdast-util-from-markdown";
import type { Extension, State, Tokenizer } from "micromark-util-types";
import type { Plugin } from "unified";
import { memo } from "react";
import { Markdown } from "./ui";

declare module "micromark-util-types" {
  interface TokenTypeMap {
    threadDelimiter: "threadDelimiter";
  }
}

declare module "mdast" {
  interface Data {
    threadDelimiter?: Delimiter;
  }
}

type Delimiter = { name?: string; partial?: boolean };
type Options = { text: string; streaming: boolean };
const OPEN = '<thread name="';
const CLOSE = "</thread>";

function delimiter(line: string, streaming: boolean): Delimiter | undefined {
  const tag = line.trim();
  const name = /^<thread name="([^"\r\n]+)">$/.exec(tag)?.[1].trim();
  if (name) return { name };
  if (tag === CLOSE) return {};
  if (
    streaming &&
    tag &&
    (CLOSE.startsWith(tag) ||
      OPEN.startsWith(tag) ||
      (tag.startsWith(OPEN) && !tag.includes(">")))
  )
    return { partial: true };
}

/** A recognized delimiter can be nested in a list item. Lift it to the document
 * level, splitting each ancestor around it so content stays on the correct side
 * of the section boundary. Only structural containers can be split: definitions
 * and other semantic nodes stay atomic, including their literal thread tags. */
function liftDelimiters(node: RootContent): RootContent[] {
  if (node.data?.threadDelimiter?.partial) return [];
  if (
    node.type !== "list" &&
    node.type !== "listItem" &&
    node.type !== "blockquote"
  )
    return [node];
  const parts: RootContent[] = [];
  let children: RootContent[] = [];
  let changed = false;
  let emitted = false;
  let firstChild = 0;
  const flush = () => {
    if (!children.length) return;
    const fragment = { ...node, children } as typeof node;
    if (node.type === "list" && fragment.type === "list" && node.ordered)
      fragment.start = (node.start ?? 1) + firstChild;
    // A continuation of the same task item must not repeat its checkbox.
    if (fragment.type === "listItem" && emitted) fragment.checked = undefined;
    parts.push(fragment);
    emitted = true;
    children = [];
  };
  for (const [index, child] of node.children.entries()) {
    const lifted = liftDelimiters(child);
    if (lifted.length !== 1 || lifted[0] !== child) changed = true;
    for (const part of lifted) {
      if (part.data?.threadDelimiter) {
        changed = true;
        flush();
        parts.push(part);
      } else {
        if (!children.length) firstChild = index;
        children.push(part);
      }
    }
  }
  if (!changed) return [node];
  flush();
  return parts;
}

/** Register a flow construct, so Markdown decides whether a line is prose or
 * literal code (including fences nested in lists and quotes). A delimiter ends
 * a Markdown block but leaves the whole reply in one document for references. */
const remarkThreadSections: Plugin<[Options], Root> = function ({
  text,
  streaming,
}) {
  const tokenize: Tokenizer = function (effects, ok, nok) {
    const offset = this.now().offset;
    const lineStart =
      Math.max(
        text.lastIndexOf("\n", offset - 1),
        text.lastIndexOf("\r", offset - 1),
      ) + 1;
    const newline = text.indexOf("\n", offset);
    const carriage = text.indexOf("\r", offset);
    const lineEnd = Math.min(
      newline === -1 ? text.length : newline,
      carriage === -1 ? text.length : carriage,
    );
    // A quote/list marker before '<' makes this an example, not a protocol tag.
    if (
      !/^[ \t]*$/.test(text.slice(lineStart, offset)) ||
      !delimiter(
        text.slice(offset, lineEnd),
        streaming && lineEnd === text.length,
      )
    )
      return nok;

    const consume: State = (code) => {
      // Micromark represents CRLF, LF and CR with these virtual character codes.
      if (code === null || code === -3 || code === -4 || code === -5) {
        effects.exit("threadDelimiter");
        return ok(code);
      }
      effects.consume(code);
      return consume;
    };
    return (code) => {
      effects.enter("threadDelimiter");
      return consume(code);
    };
  };
  const fromMarkdown: FromMarkdownExtension = {
    enter: {
      threadDelimiter(token) {
        const value = this.sliceSerialize(token);
        this.enter(
          {
            type: "html",
            value,
            data: { threadDelimiter: delimiter(value, streaming) },
          },
          token,
        );
      },
    },
    exit: {
      threadDelimiter(token) {
        this.exit(token);
      },
    },
  };
  const data = this.data() as {
    micromarkExtensions?: Extension[];
    fromMarkdownExtensions?: FromMarkdownExtension[];
  };
  (data.micromarkExtensions ??= []).push({
    flow: { 60: { name: "threadDelimiter", tokenize } },
  });
  (data.fromMarkdownExtensions ??= []).push(fromMarkdown);

  return (tree) => {
    const sections: RootContent[] = [];
    let name: string | undefined;
    let body: RootContent[] = [];
    const flush = () => {
      if (name !== undefined || body.length) {
        const content: Blockquote = {
          type: "blockquote",
          children: body as Blockquote["children"],
          data: { hName: "div", hProperties: { className: ["markdown"] } },
        };
        sections.push(
          name === undefined
            ? content
            : {
                type: "blockquote",
                data: {
                  hName: "section",
                  hProperties: {
                    className: ["thread-message"],
                    ariaLabel: name,
                  },
                },
                children: [
                  {
                    type: "paragraph",
                    children: [{ type: "text", value: name }],
                    data: {
                      hName: "div",
                      hProperties: { className: ["thread-message-name"] },
                    },
                  },
                  content,
                ],
              },
        );
      }
      body = [];
    };
    for (const node of tree.children.flatMap(liftDelimiters)) {
      const tag = node.data?.threadDelimiter;
      if (tag) {
        if (tag.partial) continue;
        flush();
        name = tag.name;
      } else {
        body.push(node);
      }
    }
    flush();
    tree.children = sections;
  };
};

export const ThreadMessage = memo(function ThreadMessage({
  text,
  streaming,
}: Options) {
  if (!text.includes("<")) return <Markdown text={text} />;
  return (
    <Markdown
      text={text}
      className="thread-sections"
      remarkPlugins={[[remarkThreadSections, { text, streaming }]]}
    />
  );
});
