import { describe, expect, it } from "vitest";
import {
  cliArgs,
  nonInteractiveArgs,
  externalUrl,
  workspaceArgs,
} from "../desktop/validation";
import { parseHelp, discoverCatalog } from "../desktop/catalog";
import {
  commandArguments,
  parameters,
  words,
  commands,
} from "../shared/commands";
import {
  blocksOf,
  mergeEvents,
  foldRoutineBlocks,
  routineRuns,
} from "../shared/transcript";
import type { HubEvent } from "../shared/types";
import { approvalShortcut } from "../shared/attention";
import { numberSetting } from "../shared/settings";
import { summaryDue, settledSummary } from "../shared/assistance";
import { localWorkingDirectory } from "../shared/paths";

describe("review regressions", () => {
  it("never uses a remote runner's worktree as a local terminal working directory", () => {
    const thread = {
      box_name: "gpu",
      worktree: "/remote/repo",
      repo: "/remote",
    } as any;
    expect(localWorkingDirectory(thread, "/Users/test")).toBe("/Users/test");
    expect(
      localWorkingDirectory({ ...thread, box_name: "local" }, "/Users/test"),
    ).toBe("/remote/repo");
  });
  it("number shortcuts cannot answer an incomplete or unversioned request", () => {
    const item = {
      attn_id: "request",
      revision: 42,
      incomplete: true,
      thread: "api",
      reason: "permission",
      summary: "preview",
      options: ["allow", "deny"],
    };
    const key = {
      key: "1",
      ctrlKey: false,
      metaKey: false,
      altKey: false,
      shiftKey: false,
    };
    expect(approvalShortcut(item, false, key)).toBeUndefined();
    expect(
      approvalShortcut({ ...item, incomplete: false, revision: 0 }, false, key),
    ).toBeUndefined();
    expect(
      approvalShortcut({ ...item, incomplete: false }, true, key),
    ).toBeUndefined();
    expect(
      approvalShortcut({ ...item, incomplete: false }, false, {
        ...key,
        metaKey: true,
      }),
    ).toBeUndefined();
    expect(approvalShortcut({ ...item, incomplete: false }, false, key)).toBe(
      "allow",
    );
  });
  it("a dropped, failed or foreign summary stays due after its minimum interval", () => {
    const attempt = { at: 1000, next: 60, basis: "" };
    expect(summaryDue(attempt, "42", 60, 60000)).toBe(false);
    expect(summaryDue(attempt, "42", 60, 61000)).toBe(true);
    const foreign = settledSummary(
      attempt,
      "42",
      { covers_in: "another-conversation", covers: 42 },
      "current",
    );
    expect(summaryDue(foreign, "42", 60, 61000)).toBe(true);
    const failed = settledSummary(
      attempt,
      "42",
      { reason: "failed: transient", next_after_secs: 90 },
      "current",
    );
    expect(summaryDue(failed, "42", 60, 91000)).toBe(true);
    const success = settledSummary(
      attempt,
      "42",
      { covers_in: "current", covers: 42 },
      "current",
    );
    expect(summaryDue(success, "42", 60, 3600000)).toBe(false);
    expect(summaryDue(success, "43", 60, 61000)).toBe(true);
  });
  it("cleared or invalid numeric settings never become zero", () => {
    for (const value of ["", " ", "NaN", "Infinity", "1.5", -1, undefined])
      expect(() => numberSetting(value)).toThrow();
    expect(numberSetting("0")).toBe(0);
    expect(numberSetting("30")).toBe(30);
  });
});

describe("routine update presentation", () => {
  const update = (seq: number, routine: boolean, thread = "api"): HubEvent => ({
    id: String(seq),
    seq,
    kind: "conversation.update",
    thread: "orchestrator-a",
    ts: "2026-11-05T12:00:00Z",
    data: { about_thread: thread, text: "update " + seq },
    routine,
  });
  it("keeps decisions visible and binds summaries to their exact conversation and run", () => {
    const events = [
      update(1, true),
      update(2, false),
      update(3, true),
      update(4, true),
    ];
    expect(
      routineRuns(events).map((run) => [
        run.live,
        run.events.map((event) => event.seq),
      ]),
    ).toEqual([
      [false, [1]],
      [true, [3, 4]],
    ]);
    const summaries = [
      {
        thread: "api",
        covers_in: "orchestrator-a",
        covers: 1,
        summary: "Old run",
      },
      {
        thread: "api",
        covers_in: "orchestrator-b",
        covers: 4,
        summary: "Wrong conversation",
      },
    ];
    let blocks = foldRoutineBlocks(blocksOf(events), events, summaries);
    expect(blocks.map((block) => block.text)).toEqual([
      "Old run",
      "update 2",
      "update 4",
    ]);
    expect(blocks[1].children).toBeUndefined();
    expect(blocks[2].children?.length).toBe(2);
    blocks = foldRoutineBlocks(blocksOf(events), events, [
      ...summaries,
      {
        thread: "api",
        covers_in: "orchestrator-a",
        covers: 3,
        summary: "Working on API",
      },
    ]);
    expect(blocks.at(-1)?.text).toBe("Working on API");
  });
});

describe("desktop authority boundaries", () => {
  it("pins CLI operations to their workspace and rejects identity and update entry points", () => {
    for (const args of [
      ["thread", "list", "--socket", "/tmp/foreign"],
      ["thread", "list", "--workspace=other"],
      ["attach", "api", "-w", "other"],
      ["mcp"],
      ["claude-memory-guard"],
      ["update"],
      ["hub"],
    ])
      expect(() => cliArgs(args)).toThrow();
    expect(cliArgs(["send", "api", "$(touch /tmp/not-executed)"])).toEqual([
      "send",
      "api",
      "$(touch /tmp/not-executed)",
    ]);
    for (const args of [
      ["hub"],
      ["attach", "api"],
      ["events", "api", "--follow"],
      ["account", "login", "codex", "work"],
    ])
      expect(() => nonInteractiveArgs(args)).toThrow();
    expect(nonInteractiveArgs(["ws", "new", "client"])).toEqual([
      "ws",
      "new",
      "client",
    ]);
  });
  it("does not open local files, script URLs or arbitrary protocol handlers from model output", () => {
    for (const url of [
      "javascript:alert(1)",
      "file:///etc/passwd",
      "twr://delete",
      "data:text/html,test",
    ])
      expect(() => externalUrl(url)).toThrow();
    expect(externalUrl("https://example.com/docs")).toBe(
      "https://example.com/docs",
    );
  });
});
describe("CLI parity catalog", () => {
  it("preserves all-remote-workspace speech while pinning local commands", () => {
    expect(workspaceArgs(["remote", "server"], "local-client")).toEqual([
      "remote",
      "server",
    ]);
    expect(workspaceArgs(["info"], "client")).toEqual(["-w", "client", "info"]);
    expect(workspaceArgs(["speak", "--all-workspaces"], "client")).toEqual([
      "speak",
      "--all-workspaces",
    ]);
  });
  it("passes leading-dash option values literally without confusing them with workspace flags", () => {
    const { command } = parseHelp(
      ["respond"],
      "Respond\n\nUsage: twr respond [OPTIONS] <TARGET> <ANSWER>\n\nArguments:\n  <TARGET>  Request\n  <ANSWER>  Answer\n\nOptions:\n      --message <MESSAGE>  Note\n",
    );
    const args = commandArguments(command, {
      target: "request",
      answer: "allow",
      message: "-window: keep this punctuation",
    });
    expect(args).toEqual([
      "respond",
      "--message=-window: keep this punctuation",
      "request",
      "allow",
    ]);
    expect(cliArgs(args)).toEqual(args);
    expect(
      cliArgs([
        "box",
        "add",
        "container",
        "--command",
        "tool",
        "--workspace",
        "container-workspace",
      ]),
    ).toContain("container-workspace");
  });
  it("keeps every selected thread in a multi-thread delete", () => {
    const command = commands.find((command) => command.id === "delete")!;
    expect(parameters(command, { threads: '"api" "web" "docs"' })).toEqual({
      threads: ["api", "web", "docs"],
    });
  });
  const help = `Create a thread\n\nUsage: twr thread new [OPTIONS] <PROJECT>\n\nArguments:\n  <PROJECT>  Repository\n\nOptions:\n      --runtime <RUNTIME>  Agent runtime\n      --alias <ALIAS>      Extra name\n      --no-worktree        Use checkout\n  -w, --workspace <NAME>   Workspace\n  -h, --help               Print help\n`;
  it("turns flags and positionals into shell-free form arguments", () => {
    const { command } = parseHelp(["thread", "new"], help);
    expect(command.fields.map((f) => f.key)).toEqual([
      "project",
      "runtime",
      "alias",
      "no-worktree",
    ]);
    const args = commandArguments(command, {
      project: "/a path/repo",
      runtime: "codex",
      alias: "api backend",
      "no-worktree": true,
    });
    expect(args).toEqual([
      "thread",
      "new",
      "--runtime",
      "codex",
      "--alias",
      "api",
      "--alias",
      "backend",
      "--no-worktree",
      "/a path/repo",
    ]);
  });
  it("keeps a trailing command after -- and preserves literal shell metacharacters", () => {
    const { command } = parseHelp(
      ["job", "run"],
      `Run\n\nUsage: twr job run [OPTIONS] -- <ARGV>...\n\nArguments:\n  <ARGV>...  Command\n\nOptions:\n      --thread <THREAD>  Thread\n`,
    );
    expect(
      commandArguments(command, {
        thread: "api",
        argv: 'printf "hello world" "$HOME"',
      }),
    ).toEqual([
      "job",
      "run",
      "--thread",
      "api",
      "--",
      "printf",
      "hello world",
      "$HOME",
    ]);
  });
  it("discovers subcommands instead of hardcoding the CLI surface", async () => {
    const tree: Record<string, string> = {
      "--help":
        "twr\n\nUsage: twr [COMMAND]\n\nCommands:\n  thread  Threads\n  help    Help",
      "thread --help":
        "Threads\n\nUsage: twr thread <COMMAND>\n\nCommands:\n  new  Create\n  list  List",
      "thread new --help": help,
      "thread list --help":
        "List\n\nUsage: twr thread list [OPTIONS]\n\nOptions:\n      --all  All threads",
    };
    const list = await discoverCatalog(async (args) => tree[args.join(" ")]);
    expect(list.map((c) => c.cli?.join(" "))).toEqual([
      "thread list",
      "thread new",
    ]);
  });
  it("does not disable default worktrees just because an optional checkbox is unchecked", () => {
    const command = commands.find((c) => c.id === "new")!;
    expect(
      parameters(command, {
        project: "/repo",
        runtime: "codex",
        worktree: false,
        no_worktree: false,
      }),
    ).toEqual({ project: "/repo", runtime: "codex" });
  });
  it("encodes speech settings using the hub value contract", () => {
    const command = commands.find((c) => c.id === "speech-control")!;
    expect(
      parameters(command, { op: "rate", rate: "180", live: false }),
    ).toEqual({ op: "rate", value: 180 });
    expect(parameters(command, { op: "live", live: false })).toEqual({
      op: "live",
      value: false,
    });
  });
  it("parses next-line box help and does not offer automatic confirmation flags", () => {
    const { command } = parseHelp(
      ["box", "add"],
      `Add a box\n\nUsage: twr box add [OPTIONS] <NAME> [SSH]...\n\nArguments:\n  <NAME>\n          Box name\n  [SSH]...\n          SSH destination\n\nOptions:\n      --command <ARGV>...\n          Container command\n  -y, --yes\n          Skip the confirmation\n`,
    );
    expect(command.fields.map((f) => f.key)).toEqual([
      "name",
      "ssh",
      "command",
    ]);
    expect(
      commandArguments(command, {
        name: "gpu",
        command: "docker exec -i test",
      }),
    ).toEqual([
      "box",
      "add",
      "gpu",
      "--command",
      "docker",
      "exec",
      "-i",
      "test",
    ]);
    expect(() => commandArguments(command, { ssh: "some-host" })).toThrow(
      "name is required",
    );
  });
  it("preserves message quotes and newlines, and cannot shift a message into a missing thread", () => {
    const { command } = parseHelp(
      ["send"],
      `Send\n\nUsage: twr send [THREAD] [PROMPT]...\n\nArguments:\n  [THREAD]  Destination\n  [PROMPT]...  Message words are joined with spaces\n`,
    );
    const prompt = `Please don't remove "this".\nKeep every line.`;
    expect(() => commandArguments(command, { prompt })).toThrow(
      "earlier positional",
    );
    expect(commandArguments(command, { thread: "api", prompt })).toEqual([
      "send",
      "api",
      prompt,
    ]);
    expect(
      commandArguments(command, { thread: "api", prompt: "--literal" }),
    ).toEqual(["send", "--", "api", "--literal"]);
  });
  it("permits subprocess workspace flags only after the explicit command separator", () => {
    expect(() => cliArgs(["attach", "api", "-wother"])).toThrow();
    expect(
      cliArgs(["job", "run", "--", "npm", "test", "--workspace=api"]),
    ).toEqual(["job", "run", "--", "npm", "test", "--workspace=api"]);
  });
});
describe("streamed transcript presentation", () => {
  const event = (seq: number, kind: string, data: any): HubEvent => ({
    id: "event-" + seq,
    seq,
    thread: "api",
    turn: 1,
    kind,
    data,
    ts: "2026-11-05T00:00:00Z",
  });
  it("replaces token deltas with the final message without duplicating it", () => {
    const blocks = blocksOf([
      event(1, "turn.started", { from: "user", prompt: "Hello" }),
      event(2, "agent.text", { delta: true, text: "Hi " }),
      event(3, "agent.text", { delta: true, text: "there" }),
      event(4, "agent.text", { final: true, text: "Hi there!" }),
    ]);
    expect(blocks.map((b) => [b.kind, b.text])).toEqual([
      ["user", "Hello"],
      ["assistant", "Hi there!"],
    ]);
    expect(blocks[1].pending).toBe(false);
  });
  it("deduplicates replay at a live subscription boundary", () => {
    const a = event(1, "agent.text", { text: "one" }),
      b = event(2, "agent.text", { text: "two" });
    expect(mergeEvents([b], [a, b]).map((e) => e.seq)).toEqual([1, 2]);
  });
  it("groups quoted arguments without shell expansion or execution", () => {
    expect(words(`sh -c 'echo hello' "a b" ''`)).toEqual([
      "sh",
      "-c",
      "echo hello",
      "a b",
      "",
    ]);
    expect(() => words('"unterminated')).toThrow();
  });
  it("finishes pending tool blocks when a turn is interrupted", () => {
    const blocks = blocksOf([
      event(1, "agent.tool_call", {
        call_id: "x",
        tool: "Bash",
        input_summary: "test",
      }),
      event(2, "turn.ended", { result: "interrupted" }),
    ]);
    expect(blocks[0].pending).toBe(false);
    expect(blocks[1].text).toBe("Turn interrupted");
  });
});
