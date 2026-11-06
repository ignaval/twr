import type { HubEvent } from "./types";
export interface Block {
  id: string;
  kind: "user" | "assistant" | "tool" | "thinking" | "notice";
  text: string;
  detail?: string;
  seq: number;
  turn?: number;
  ts: string;
  pending?: boolean;
  about?: string;
  routine?: boolean;
  children?: Block[];
}
/** Presentation only: collapse token deltas and pair tool results. Authority and thread
 * state are exclusively the Rust fold and hub. Never infer permission decisions here. */
export function blocksOf(events: HubEvent[]): Block[] {
  const output: Block[] = [];
  const calls = new Map<string, Block>();
  let streaming: Block | undefined;
  for (const event of events) {
    const data = event.data || {};
    const base = {
      id: event.id,
      seq: event.seq,
      turn: event.turn,
      ts: event.ts,
    };
    switch (event.kind) {
      case "turn.started":
        streaming = undefined;
        if (data.prompt)
          output.push({
            ...base,
            kind: data.from === "user" ? "user" : "notice",
            text: data.prompt,
          });
        break;
      case "agent.text":
        if (data.delta) {
          if (!streaming || streaming.turn !== event.turn) {
            streaming = { ...base, kind: "assistant", text: "", pending: true };
            output.push(streaming);
          }
          streaming.text += data.text || "";
        } else if (data.final && streaming && streaming.turn === event.turn) {
          streaming.seq = event.seq;
          streaming.text = data.text || streaming.text;
          streaming.pending = false;
          streaming = undefined;
        } else {
          if (streaming) streaming.pending = false;
          streaming = undefined;
          if (data.text)
            output.push({ ...base, kind: "assistant", text: data.text });
        }
        break;
      case "agent.tool_call": {
        streaming = undefined;
        const block: Block = {
          ...base,
          kind: "tool",
          text: data.tool || "Tool",
          detail: data.input_summary || "",
          pending: true,
        };
        calls.set(data.call_id, block);
        output.push(block);
        break;
      }
      case "agent.tool_result": {
        const block = calls.get(data.call_id);
        if (block) {
          block.pending = false;
          if (data.ok === false) block.text += " · failed";
          block.detail += data.output_summary
            ? "\n\n" + data.output_summary
            : "";
        }
        break;
      }
      case "agent.thinking":
        if (data.text)
          output.push({ ...base, kind: "thinking", text: data.text });
        break;
      case "turn.ended":
        if (streaming) streaming.pending = false;
        streaming = undefined;
        for (const call of calls.values())
          if (call.turn === event.turn) call.pending = false;
        if (data.result === "interrupted")
          output.push({ ...base, kind: "notice", text: "Turn interrupted" });
        if (data.result === "error")
          output.push({
            ...base,
            kind: "notice",
            text:
              data.error || "This turn failed. Check the thread for details.",
          });
        break;
      case "conversation.update":
        if (data.text || data.summary || data.message)
          output.push({
            ...base,
            kind: "notice",
            text: data.text || data.summary || data.message,
            about: data.about_thread,
            routine: event.routine,
          });
        break;
      case "context.compacting":
        output.push({ ...base, kind: "notice", text: "Compacting context…" });
        break;
      case "context.compacted":
        output.push({ ...base, kind: "notice", text: "Context compacted" });
        break;
      case "prompt.withdrawn":
        output.push({ ...base, kind: "notice", text: "Message taken back" });
        break;
    }
  }
  return output;
}
export interface RoutineRun {
  thread: string;
  events: HubEvent[];
  live: boolean;
}
export function routineRuns(events: HubEvent[]): RoutineRun[] {
  const runs: RoutineRun[] = [];
  const open = new Map<string, RoutineRun>();
  for (const event of events) {
    if (event.kind !== "conversation.update" || !event.data.about_thread)
      continue;
    const thread = event.data.about_thread;
    if (!event.routine) {
      const run = open.get(thread);
      if (run) run.live = false;
      open.delete(thread);
      continue;
    }
    let run = open.get(thread);
    if (!run) {
      run = { thread, events: [], live: true };
      runs.push(run);
      open.set(thread, run);
    }
    run.events.push(event);
  }
  return runs;
}
/** Fold only updates already classified by Rust. Non-routine requests keep their place. */
export function foldRoutineBlocks(
  blocks: Block[],
  events: HubEvent[],
  summaries: import("./assistance").Summary[],
): Block[] {
  const groups = new Map<
    number,
    { newest: number; blocks: Block[]; text: string }
  >();
  const bySeq = new Map(blocks.map((block) => [block.seq, block]));
  for (const run of routineRuns(events)) {
    const entries = run.events
      .map((event) => bySeq.get(event.seq))
      .filter((block): block is Block => !!block);
    if (!entries.length) continue;
    const covered = new Set(run.events.map((event) => event.seq));
    const summary = summaries.findLast(
      (value) =>
        value.thread === run.thread &&
        value.covers_in === events[0]?.thread &&
        value.covers != null &&
        covered.has(value.covers),
    );
    const newest = entries.at(-1)!;
    const group = {
      newest: newest.seq,
      blocks: entries,
      text: summary?.summary || newest.text,
    };
    for (const entry of entries) groups.set(entry.seq, group);
  }
  return blocks.flatMap((block) => {
    const group = groups.get(block.seq);
    if (!group) return [block];
    if (group.newest !== block.seq) return [];
    return [
      {
        ...block,
        id: "routine:" + group.blocks[0].id,
        text: group.text,
        children: group.blocks,
      },
    ];
  });
}
export function mergeEvents(
  current: HubEvent[],
  incoming: HubEvent[],
): HubEvent[] {
  if (!incoming.length) return current;
  if (!current.length) return incoming;
  if (
    current[current.length - 1].seq < incoming[0].seq &&
    incoming.every((e, i) => i === 0 || e.seq > incoming[i - 1].seq)
  )
    return [...current, ...incoming];
  const map = new Map(current.map((e) => [e.seq, e]));
  for (const event of incoming) map.set(event.seq, event);
  return [...map.values()].sort((a, b) => a.seq - b.seq);
}
