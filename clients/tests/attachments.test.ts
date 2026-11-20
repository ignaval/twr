import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import {
  attachmentLines,
  fileKind,
  fileSize,
  splitAttachments,
  withAttachments,
} from "../shared/attachmentText";
import {
  QueuedText,
  SentMessage,
  attachmentStatus,
  attachmentWait,
  uploadPercent,
} from "../shared/ComposerAttachments";
import type { Attachment } from "../shared/uploads";

const named = [
  { name: "shot.png", path: "/home/me/.twr-uploads/a/shot.png" },
  { name: "notes.txt", path: "/home/me/.twr-uploads/b/notes.txt" },
];

describe("a message's attachment lines", () => {
  it("names each file after the text, in the format the agents read", () => {
    expect(withAttachments("see these", named)).toBe(
      "see these\nUploaded file: shot.png\n/home/me/.twr-uploads/a/shot.png\nUploaded file: notes.txt\n/home/me/.twr-uploads/b/notes.txt",
    );
    expect(withAttachments("trailing\n\n", named.slice(0, 1))).toBe(
      "trailing\nUploaded file: shot.png\n/home/me/.twr-uploads/a/shot.png",
    );
    expect(withAttachments("", named.slice(1))).toBe(
      attachmentLines(named.slice(1)),
    );
    expect(withAttachments("no files", [])).toBe("no files");
  });

  it("reads them back out, and only the exact two-line form", () => {
    const sent = withAttachments("look", named);
    expect(splitAttachments(sent)).toEqual({ body: "look", files: named });
    expect(splitAttachments(attachmentLines(named))).toEqual({
      body: "",
      files: named,
    });
    // The old draft format: the pair in the middle of what was typed.
    expect(
      splitAttachments(
        "before\nUploaded file: a.txt\nC:\\Users\\me\\a.txt\nafter",
      ),
    ).toEqual({
      body: "before\nafter",
      files: [{ name: "a.txt", path: "C:\\Users\\me\\a.txt" }],
    });
    for (const text of [
      "Uploaded file: a.txt",
      "Uploaded file: a.txt\nrelative/a.txt",
      "Uploaded file: a.txt\n /indented/a.txt",
      "Uploaded file: \n/a.txt",
      "Uploaded file: a.txt\n\n/a.txt",
      "I said Uploaded file: a.txt\n/a.txt",
      "plain text",
    ])
      expect(splitAttachments(text)).toEqual({ body: text, files: [] });
  });

  it("names kinds and sizes for the chips", () => {
    expect(fileKind("a.PNG")).toBe("image");
    expect(fileKind("main.rs")).toBe("code");
    expect(fileKind("notes.md")).toBe("text");
    expect(fileKind("archive.zip")).toBe("file");
    expect(fileSize(512)).toBe("512 B");
    expect(fileSize(4300)).toBe("4.2 KiB");
    expect(fileSize(3.1 * 1024 * 1024)).toBe("3.1 MiB");
  });
});

describe("attachment status", () => {
  const item = (fields: Partial<Attachment>): Attachment => ({
    id: "1",
    file: { name: "big.bin", size: 1000 } as File,
    thread: "t",
    workspace: "w",
    phase: "uploading",
    loaded: 0,
    ...fields,
  });
  it("says how far an upload is, never 100% before it is done", () => {
    expect(uploadPercent(item({ loaded: 425 }))).toBe(42);
    expect(uploadPercent(item({ loaded: 1000 }))).toBe(99);
    expect(uploadPercent(item({ phase: "ready", loaded: 1000 }))).toBe(100);
    expect(attachmentStatus(item({ loaded: 500 }))).toBe("Uploading 50%");
    expect(attachmentStatus(item({ phase: "ready" }))).toBe("1000 B · ready");
  });
  it("says why Send waits", () => {
    expect(attachmentWait([item({})])).toContain("finish uploading");
    expect(attachmentWait([item({}), item({ phase: "error" })])).toBe(
      "Resume or remove big.bin to send: a message waits for its files",
    );
  });
});

describe("a sent message's files", () => {
  it("render as chips with the name only, and the text without them", () => {
    const html = renderToStaticMarkup(
      createElement(SentMessage, {
        text: withAttachments("look at these", named),
        sentImage: (path) =>
          path.endsWith("shot.png") ? "blob:preview" : undefined,
      }),
    );
    expect(html).toContain("look at these");
    expect(html).toContain('aria-label="Attached files"');
    expect(html.match(/<li[ >]/g)).toHaveLength(2);
    expect(html).toContain(">shot.png</span>");
    expect(html).toContain(">notes.txt</span>");
    expect(html).toContain('src="blob:preview"');
    expect(html).not.toContain(".twr-uploads");
    expect(html).not.toContain("Uploaded file");
  });
  it("leave a message without the exact form as it is", () => {
    const text = "Uploaded file: notes\nnot a path";
    const html = renderToStaticMarkup(createElement(SentMessage, { text }));
    expect(html).not.toContain("sent-attachments");
    expect(html).toContain("not a path");
  });
  it("show a message of files only as its chips", () => {
    const html = renderToStaticMarkup(
      createElement(SentMessage, { text: attachmentLines(named) }),
    );
    expect(html).not.toContain("message-body");
    expect(html.match(/<li[ >]/g)).toHaveLength(2);
  });
  it("read as names on a queued message's line", () => {
    const html = renderToStaticMarkup(
      createElement(QueuedText, { text: withAttachments("later", named) }),
    );
    expect(html).toContain("later");
    expect(html).toContain("shot.png, notes.txt");
    expect(html).not.toContain("/home/me");
  });
});
