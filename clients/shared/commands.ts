/** Typed user operations. The complete CLI catalog is also discovered from the bundled
 * binary's help, so newly added CLI flags remain reachable without hand-maintained copies. */
export interface Field {
  key: string;
  label: string;
  type?:
    | "text"
    | "textarea"
    | "number"
    | "boolean"
    | "directory"
    | "select"
    | "words";
  required?: boolean;
  options?: string[];
  placeholder?: string;
  description?: string;
  initial?: any;
  advanced?: boolean;
}
export interface Command {
  id: string;
  title: string;
  description: string;
  group: string;
  method?: string;
  fields: Field[];
  confirm?: string;
  cli?: string[];
  cliFields?: {
    key: string;
    flag?: string;
    many?: boolean;
    separator?: boolean;
    literal?: boolean;
  }[];
}
const f = (key: string, label: string, extra: Partial<Field> = {}): Field => ({
  key,
  label,
  ...extra,
});
const thread = f("thread", "Thread", { required: true });
const runtime = f("runtime", "Runtime", {
  placeholder: "claude-code, codex, grok-build, or a configured runtime",
  required: true,
});
const account = f("account", "Account", { required: true });
const cmd = (
  id: string,
  title: string,
  group: string,
  method: string,
  fields: Field[] = [],
  description = "",
  confirm?: string,
): Command => ({ id, title, group, method, fields, description, confirm });
export const commands: Command[] = [
  cmd(
    "conversation-search",
    "Search saved conversations",
    "History",
    "conversation.search",
    [
      f("query", "Search text", { required: true }),
      f("thread", "Thread (optional)"),
      f("before", "Before event ID"),
      f("limit", "Results (1–100)", { type: "number", initial: 30 }),
    ],
    "Search messages, tool output and decisions in the selected workspace, including archived output.",
  ),
  cmd(
    "event-context",
    "Read exact conversation event",
    "History",
    "thread.event_context",
    [
      thread,
      f("event_id", "Event ID", { required: true }),
      f("before", "Earlier events (0–30)", { type: "number", initial: 12 }),
      f("after", "Later events (0–30)", { type: "number", initial: 12 }),
    ],
    "Read the selected event and its surrounding history without changing the message destination.",
  ),
  cmd("device-list", "Paired devices", "Devices", "device.list"),
  cmd(
    "device-revoke",
    "Revoke device access",
    "Devices",
    "device.revoke",
    [f("id", "Device ID", { required: true })],
    "Disconnect this device from your hubs.",
    "Revoke this device's access?",
  ),
  cmd("device-pair", "Pair a mobile device", "Devices", "device.pair_begin", [
    f("scopes", "Scopes", {
      type: "words",
      required: true,
      initial: "read operate",
      description:
        "read operate; add terminal or admin only for a device you trust",
    }),
  ]),

  cmd(
    "new",
    "New thread",
    "Threads",
    "thread.new",
    [
      f("project", "Project or repository", {
        required: true,
        type: "directory",
        placeholder: "Project name or /path/to/repository",
      }),
      f("name", "Name", { placeholder: "e.g. api-health-check" }),
      { ...runtime, required: false },
      f("box", "Box", { placeholder: "local" }),
      f("model", "Model"),
      f("effort", "Reasoning effort"),
      f("mode", "Permission mode", {
        type: "select",
        options: [
          "auto",
          "plan",
          "manual",
          "acceptEdits",
          "read-only",
          "dontAsk",
          "bypassPermissions",
        ],
      }),
      f("account", "Account or account pool"),
      f("aliases", "Aliases", { type: "words" }),
      f("base", "Base branch or commit"),
      f("workspace_mode", "Working directory", {
        type: "select",
        options: ["default", "separate", "shared"],
        initial: "default",
      }),
      f("outside_pin", "Allow an account outside project pins", {
        type: "boolean",
      }),
    ],
    "Give an agent its own thread and working directory.",
  ),
  cmd("mode", "Change permission mode", "Threads", "thread.set_mode", [
    thread,
    f("mode", "Permission mode", {
      required: true,
      type: "text",
      options: [
        "auto",
        "plan",
        "manual",
        "acceptEdits",
        "read-only",
        "dontAsk",
        "bypassPermissions",
      ],
    }),
  ]),
  cmd(
    "model",
    "Change model, runtime or account",
    "Threads",
    "thread.set_model",
    [
      thread,
      f("runtime", "Runtime", {
        placeholder: "Keep current, or claude-code / codex / grok-build",
      }),
      f("model", "Model", { placeholder: "Keep current, or default" }),
      f("effort", "Reasoning effort", {
        placeholder: "Keep current, or default",
      }),
      f("account", "Account or pool"),
      f("outside_pin", "Allow outside account pins", { type: "boolean" }),
    ],
  ),
  cmd("rename", "Rename thread", "Threads", "thread.rename", [
    thread,
    f("name", "New name", { required: true }),
  ]),
  cmd(
    "stop",
    "Stop thread",
    "Threads",
    "thread.stop",
    [thread],
    "Interrupt its runtime and drop queued prompts.",
    "Stop this thread and drop its queued messages?",
  ),
  cmd(
    "park",
    "Park thread",
    "Threads",
    "thread.park",
    [
      thread,
      f("keep", "Keep the worktree on disk", { type: "boolean" }),
      f("force", "Interrupt running work", { type: "boolean" }),
    ],
    "Keep the session and branch; remove a clean worktree.",
    "Park this thread? Its runtime will stop.",
  ),
  cmd("resume", "Resume thread", "Threads", "thread.resume", [thread]),
  cmd(
    "trim",
    "Trim build output",
    "Threads",
    "thread.trim",
    [thread],
    "Remove disposable, ignored build output.",
    "Remove this thread’s disposable build output?",
  ),
  cmd("compact", "Compact context", "Threads", "thread.compact", [
    thread,
    f("offers", "Automatic context compaction", {
      type: "select",
      options: ["on", "off"],
    }),
  ]),
  cmd(
    "delete",
    "Move thread to trash",
    "Threads",
    "thread.delete",
    [f("threads", "Threads", { type: "words", required: true })],
    "The hub will show exactly what goes. Confirm its request to continue.",
  ),
  cmd(
    "send-after",
    "Send after other threads finish",
    "Threads",
    "thread.send",
    [
      thread,
      f("prompt", "Message", { type: "textarea", required: true }),
      f("after", "Wait for these threads", {
        type: "words",
        required: true,
        placeholder: "api web",
      }),
    ],
  ),
  cmd(
    "cancel-after",
    "Cancel a held message",
    "Threads",
    "thread.send_cancel",
    [f("id", "Held message ID", { required: true })],
  ),
  cmd("orch", "Switch orchestrator", "Orchestrator", "orchestrator.use", [
    runtime,
    f("model", "Model"),
    f("summary", "Include a handoff summary", {
      type: "boolean",
      initial: true,
    }),
    f("wait_secs", "Wait for running turn (seconds)", {
      type: "number",
      initial: 60,
    }),
  ]),
  cmd(
    "clear",
    "Fresh orchestrator conversation",
    "Orchestrator",
    "orchestrator.reset",
    [],
    "Keep workers and history; start a fresh orchestrator session.",
    "Start a fresh orchestrator conversation?",
  ),
  cmd("watch", "Watch a thread", "Orchestrator", "orchestrator.watch", [
    thread,
    f("note", "What should the orchestrator do?", {
      required: true,
      type: "textarea",
    }),
    f("repeat", "Keep watching", { type: "boolean" }),
  ]),
  cmd("unwatch", "Remove a watch", "Orchestrator", "orchestrator.unwatch", [
    f("watch", "Watch ID or all", { required: true }),
  ]),
  cmd("watches", "List watches", "Orchestrator", "orchestrator.watches"),
  cmd("account-check", "Check account login", "Accounts", "account.status", [
    f("runtime", "Runtime"),
    f("account", "Account"),
    f("refresh", "Refresh login state", { type: "boolean", initial: true }),
  ]),
  cmd("account-native", "Name a native account", "Accounts", "account.add", [
    runtime,
    account,
    f("native", "Use the runtime’s current login", {
      type: "boolean",
      initial: true,
    }),
  ]),
  cmd(
    "account-remove",
    "Remove account",
    "Accounts",
    "account.remove",
    [
      runtime,
      account,
      f("move_to", "Move its threads to account"),
      f("force", "Remove while threads still reference it", {
        type: "boolean",
      }),
    ],
    "Login files are kept. Purge and logout are available under CLI actions.",
    "Remove this account from twr?",
  ),
  cmd("box-refresh", "Refresh installed agents", "Boxes", "box.agents", [
    f("box", "Box", { required: true }),
    f("refresh", "Probe installed agents", { type: "boolean", initial: true }),
  ]),
  cmd("box-projects", "List box projects", "Boxes", "box.projects", [
    f("box", "Box", { required: true }),
  ]),
  cmd("job", "Run a job", "Jobs", "job.start", [
    thread,
    f("argv", "Command and arguments", {
      required: true,
      type: "words",
      placeholder: "cargo test --workspace",
    }),
    f("name", "Job name"),
    f("slots", "Job slots", { type: "number", initial: 1 }),
    f("timeout_secs", "Timeout (seconds)", { type: "number" }),
    f("wake", "Tell the thread when it finishes", {
      type: "boolean",
      initial: true,
    }),
  ]),
  cmd("job-log", "Read job output", "Jobs", "job.log", [
    f("job", "Job ID", { required: true }),
    f("lines", "Lines", { type: "number", initial: 200 }),
  ]),
  cmd(
    "job-cancel",
    "Cancel job",
    "Jobs",
    "job.cancel",
    [f("job", "Job ID", { required: true })],
    "Stop its process group.",
    "Cancel this job?",
  ),
  cmd("shell", "Run a shell command for a thread", "Jobs", "shell.run", [
    thread,
    f("command", "Shell command", { required: true, type: "textarea" }),
    f("share", "Share the result with the thread", { type: "boolean" }),
  ]),
  cmd(
    "context-read",
    "Read standing context",
    "Context & memory",
    "context.read",
    [f("project", "Project or thread", { required: true })],
  ),
  cmd("memory-read", "Read memory note", "Context & memory", "memory.read", [
    f("name", "Note name", { required: true }),
  ]),
  cmd("memory-save", "Write memory note", "Context & memory", "memory.save", [
    f("name", "Name", { required: true }),
    f("type", "Type", {
      type: "select",
      options: ["user", "feedback", "project", "reference"],
      required: true,
    }),
    f("description", "Description", { required: true }),
    f("body", "Note", { type: "textarea", required: true }),
    f("rev", "Existing revision (when editing)"),
  ]),
  cmd(
    "memory-forget",
    "Forget a memory note",
    "Context & memory",
    "memory.forget",
    [f("name", "Name", { required: true }), f("rev", "Revision")],
    "Remove its file from the orchestrator’s memory.",
    "Forget this note?",
  ),
  cmd("restore", "Restore from trash", "Trash", "trash.restore", [
    f("item", "Trash item ID", { required: true }),
    f("as", "Restore with a different name"),
  ]),
  cmd(
    "purge",
    "Permanently empty trash",
    "Trash",
    "trash.empty",
    [
      f("items", "Trash item IDs", { type: "words" }),
      f("all", "Every item on online boxes", { type: "boolean" }),
      f("dry_run", "Preview without deleting", {
        type: "boolean",
        initial: true,
      }),
    ],
    "Turn off preview only when you intend to permanently delete these items.",
    "Run this trash operation? Items are unrecoverable when preview is off.",
  ),
  cmd(
    "usage-threads",
    "Usage by thread, project or runtime",
    "Usage",
    "usage.threads",
    [
      f("since", "Since (for example 7d, 2026-10-01, or RFC 3339)"),
      f("by", "Group by", {
        type: "select",
        options: ["thread", "project", "runtime"],
        initial: "thread",
      }),
    ],
  ),
  cmd("read", "Read messages aloud", "Speech", "speech.request", [
    f("thread", "Thread (orchestrator by default)"),
    f("last", "Last messages", { type: "number", initial: 1 }),
    f("speaker", "Speaker"),
    f("seq", "Specific message sequence (instead of last messages)", {
      type: "number",
    }),
  ]),
  cmd("speech-control", "Control reading aloud", "Speech", "speech.control", [
    f("op", "Action", {
      type: "select",
      options: ["stop", "pause", "resume", "skip", "live", "voice", "rate"],
      required: true,
    }),
    f("speaker", "Speaker"),
    f("voice", "Voice"),
    f("rate", "Words per minute", { type: "number" }),
    f("live", "Read new messages", { type: "boolean" }),
  ]),
  cmd(
    "speech-status",
    "Speakers and reading status",
    "Speech",
    "speech.status",
  ),
  cmd("events", "Read event log", "Diagnostics", "thread.events", [
    thread,
    f("since_seq", "After sequence", { type: "number", initial: 0 }),
    f("limit", "Page size", { type: "number", initial: 200 }),
  ]),
  cmd("control", "Read control log", "Diagnostics", "control.read", [
    f("since", "After global sequence", { type: "number", initial: 0 }),
    f("limit", "Page size", { type: "number", initial: 200 }),
  ]),
  cmd("log-stats", "Storage statistics", "Maintenance", "log.stats"),
  cmd(
    "app-update",
    "Check for app updates",
    "Maintenance",
    "desktop.check_updates",
    [],
    "Check the latest release on GitHub. Desktop and engine update together through the platform installer.",
  ),
  cmd(
    "log-compact",
    "Archive old agent events",
    "Maintenance",
    "log.compact",
    [f("vacuum", "Reclaim database disk space", { type: "boolean" })],
    "Apply the configured archive and retention policy.",
    "Compact the hub’s event log?",
  ),
  cmd(
    "hub-stop",
    "Stop this workspace’s hub",
    "Maintenance",
    "hub.shutdown",
    [],
    "Stop the hub and its local agent runtimes. Closing the app does not do this.",
    "Stop this hub and its local agent runtimes?",
  ),
];

/** Shell-like argument grouping, WITHOUT expansion or execution. */
export function words(text: string): string[] {
  const result: string[] = [];
  let word = "",
    quote = "",
    escaped = false,
    active = false;
  for (const c of text) {
    if (escaped) {
      word += c;
      escaped = false;
      active = true;
    } else if (c === "\\" && quote !== "'") {
      escaped = true;
      active = true;
    } else if (quote) {
      if (c === quote) quote = "";
      else word += c;
    } else if (c === '"' || c === "'") {
      quote = c;
      active = true;
    } else if (/\s/.test(c)) {
      if (active) {
        result.push(word);
        word = "";
        active = false;
      }
    } else {
      word += c;
      active = true;
    }
  }
  if (escaped || quote)
    throw new Error("Close the quote or escape in the arguments");
  if (active) result.push(word);
  return result;
}
export function parameters(
  command: Command,
  values: Record<string, any>,
): Record<string, any> {
  const result: Record<string, any> = {};
  for (const field of command.fields) {
    const value = values[field.key];
    if (field.required && (value === undefined || value === ""))
      throw new Error(`${field.label} is required`);
    if (
      value === undefined ||
      value === "" ||
      (field.type === "boolean" &&
        value === false &&
        field.initial === undefined)
    )
      continue;
    result[field.key] =
      field.type === "number"
        ? Number(value)
        : field.type === "words"
          ? words(value)
          : value;
    if (field.type === "number" && !Number.isFinite(result[field.key]))
      throw new Error(`${field.label} must be a number`);
  }
  if (command.method === "thread.new") {
    const mode = result.workspace_mode;
    delete result.workspace_mode;
    if (mode === "separate") result.worktree = true;
    if (mode === "shared") result.no_worktree = true;
  }
  if (command.method === "speech.request" && result.seq !== undefined)
    delete result.last;
  if (command.method === "speech.control") {
    const op = result.op;
    const value =
      op === "voice"
        ? values.voice
        : op === "rate"
          ? Number(values.rate)
          : op === "live"
            ? !!values.live
            : undefined;
    delete result.voice;
    delete result.rate;
    delete result.live;
    if (value !== undefined) result.value = value;
  }
  return result;
}
export function commandArguments(
  command: Command,
  values: Record<string, any>,
): string[] {
  const args = [...command.cli!];
  const positionals: string[] = [],
    trailing: string[] = [],
    variadicOptions: string[] = [];
  let missingPositional = false;
  for (const entry of command.cliFields || []) {
    const value = values[entry.key];
    const field = command.fields.find((f) => f.key === entry.key)!;
    const empty = value === undefined || value === "" || value === false;
    if (!entry.flag && empty) missingPositional = true;
    if (field.required && empty) throw new Error(`${field.label} is required`);
    if (empty) continue;
    if (!entry.flag && missingPositional)
      throw new Error(
        "Fill earlier positional arguments first so this command cannot target the wrong thread.",
      );
    const parts =
      value === true
        ? []
        : entry.many && !entry.literal
          ? words(String(value))
          : [String(value)];
    if (entry.flag) {
      if (field.type === "boolean") args.push(entry.flag);
      else if (entry.many && entry.separator)
        variadicOptions.push(entry.flag, ...parts);
      else if (entry.many)
        for (const part of parts)
          args.push(
            ...(part.startsWith("-")
              ? [entry.flag + "=" + part]
              : [entry.flag, part]),
          );
      else if (parts[0]?.startsWith("-"))
        args.push(entry.flag + "=" + parts[0]);
      else args.push(entry.flag, ...parts);
    } else (entry.separator ? trailing : positionals).push(...parts);
  }
  const escape = positionals.some((v) => v.startsWith("-"));
  if (escape && (variadicOptions.length || trailing.length))
    throw new Error(
      "A positional value beginning with - cannot be combined with a trailing command.",
    );
  return [
    ...args,
    ...(escape ? ["--"] : []),
    ...positionals,
    ...variadicOptions,
    ...(trailing.length ? ["--", ...trailing] : []),
  ];
}
