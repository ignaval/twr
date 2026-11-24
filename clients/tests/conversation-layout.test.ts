import { readFileSync } from "node:fs";
import { join } from "node:path";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { DeleteBriefView } from "../shared/DeleteBrief";
import { attentionKindLabel } from "../shared/connectivity";
import type { DeleteBrief } from "../shared/types";

const root = join(import.meta.dirname, "..");
const css = readFileSync(join(root, "shared/style.css"), "utf8");
const mobile = readFileSync(join(root, "mobile/src/mobile.css"), "utf8");
const rule = (source: string, selector: string) => {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const match = source.match(
    new RegExp(`(?:^|\\n)\\s*${escaped} \\{([^}]*)\\}`),
  );
  if (!match) throw new Error(`no rule for ${selector}`);
  return match[1];
};

describe("the conversation's text column", () => {
  // The history's rows sit in `.transcript-virtual`: the scroller's 24px inset, then the
  // content width, centered. What belongs to the conversation lines up with it.
  const column = rule(css, ".transcript-virtual");
  it("is the content width, centered", () => {
    expect(column).toContain("max-width: var(--content-width);");
    expect(column).toContain("margin-inline: auto;");
  });

  it("holds the working cursor on its own line, at its left edge", () => {
    const line = rule(css, ".working-line");
    expect(line).toContain("max-width: var(--content-width);");
    expect(line).toContain("margin-inline: auto;");
    expect(rule(css, ".working-line .working-cursor")).toContain("left: 0;");
  });

  it("holds the shortcut to your preceding message, in a row of its own", () => {
    const bar = rule(css, ".latest-user-message");
    // Inset like the history (its 24px padding), as wide as the column.
    expect(bar).toContain("width: calc(100% - 48px);");
    expect(bar).toContain("max-width: var(--content-width);");
    expect(bar).toContain("margin-inline: auto;");
    expect(bar).toContain("flex-shrink: 0;");
    expect(bar).not.toMatch(/position: (absolute|fixed|sticky)/);
    // Within the column: a long preview truncates there.
    const button = rule(css, ".latest-user-message > button");
    expect(button).toContain("max-width: 100%;");
    // On a phone the history's inset is 12px.
    expect(rule(mobile, '[data-mobile="true"] .latest-user-message')).toContain(
      "width: calc(100% - 24px);",
    );
  });

  it("fades what scrolls under the row above it, taking no room", () => {
    const fade = rule(css, ".conversation-scroll::before");
    expect(fade).toContain("position: sticky;");
    expect(fade).toContain("height: 12px;");
    expect(fade).toContain("margin-bottom: -12px;");
    expect(fade).toContain("pointer-events: none;");
  });
});

describe("a delete card", () => {
  const brief: DeleteBrief = {
    title: "Delete 4 threads?",
    note: "This card replaces 01X: since it was raised, there is more to lose.",
    threads: [
      {
        name: "clean-a",
        project: "twr",
        risk: "no uncommitted or unpushed work",
        clean: true,
      },
      { name: "api", project: "twr", risk: "2 unpushed commits" },
      { name: "clean-b", risk: "no uncommitted or unpushed work", clean: true },
      {
        name: "web",
        risk: "3 uncommitted files; cannot be deleted now",
        refused: true,
      },
    ],
    total:
      "Deletes 1.9 GB of build output outright; the rest (160 MB) goes to the trash for 7 days.",
  };
  const summary =
    "Delete 4 threads?\n- api (project twr): stops its runtime, moves its worktree /home/u/worktrees/api (32 MB) to the trash";
  const render = (b: DeleteBrief) =>
    renderToStaticMarkup(
      createElement(DeleteBriefView, { brief: b, summary, id: "s" }),
    );

  it("says it in short, a line per thread, the whole summary behind Details", () => {
    const html = render(brief);
    expect(html).toContain(
      '<p class="delete-brief-title">Delete 4 threads?</p>',
    );
    expect(html).toContain("there is more to lose");
    expect(html).toContain("<b>api</b>");
    expect(html).toContain(
      '<span class="delete-brief-risk">2 unpushed commits</span>',
    );
    expect(html).toMatch(/<li data-refused="true"><b>web<\/b>/);
    expect(html).toContain("Deletes 1.9 GB of build output outright");
    // The threads that lose nothing share a line.
    expect(html).toContain(
      '<li class="delete-brief-clean"><b>clean-a, clean-b</b>',
    );
    expect(html.match(/<li/g)).toHaveLength(3);
    // The paths and sizes wait behind Details, collapsed.
    const brief_ = html.slice(0, html.indexOf("<details"));
    expect(brief_).not.toContain("/home/u/worktrees/api");
    expect(html).toMatch(
      /<details class="delete-details"><summary>Details<\/summary><pre class="attention-summary">Delete 4 threads\?\n- api/,
    );
    expect(html).not.toContain("<details open");
  });

  it("lists a single clean thread on its own line", () => {
    const html = render({ ...brief, threads: brief.threads.slice(0, 2) });
    expect(html).not.toContain("delete-brief-clean");
    expect(html).toContain("<b>clean-a</b>");
  });

  it("is headed as a confirmation, not a question", () => {
    expect(attentionKindLabel({ reason: "question", delete: brief })).toBe(
      "Confirm deletion",
    );
    // A card raised before the hub gave it a brief: the hub's Delete/Cancel question.
    expect(
      attentionKindLabel({
        reason: "question",
        thread: "hub",
        proposal: true,
        options: ["Delete", "Cancel"],
      }),
    ).toBe("Confirm deletion");
    expect(
      attentionKindLabel({
        reason: "question",
        thread: "hub",
        proposal: true,
        options: ["Confirm", "Cancel"],
      }),
    ).toBe("Question for you");
  });
});
