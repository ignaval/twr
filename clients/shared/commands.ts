import { ACCESS_CHOICES, FULL_ACCESS_CONFIRM } from "./orchestrator-access";
import { SINCE_CHOICES } from "./fieldChoices";
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
    | "words"
    | "model"
    | "effort"
    | PickerType;
  required?: boolean;
  options?: string[];
  /** A `model` or `effort` field's choices, from the hub's catalog (`shared/models.ts`); a
   * `select` or picker field's labelled choices (`shared/fieldChoices.ts`). */
  choices?: { value: string; label?: string; description?: string }[];
  /** A `model` or `effort` field left empty keeps the thread's; `default` clears it. A
   * picker field left empty keeps the current value ("Keep current"). */
  keep?: boolean;
  /** What a select's empty choice says, when not "Keep default"/"Choose…". */
  empty?: string;
  /** A picker whose choices are still loading: drawn as a disabled select. */
  loading?: boolean;
  placeholder?: string;
  description?: string;
  initial?: any;
  advanced?: boolean;
  /** A `boolean` field drawn as a switch (`role="switch"`): it turns a mode on. */
  switch?: boolean;
}
/** Fields drawn as a dropdown of what the hub has (`shared/fieldChoices.ts` builds the
 * choices). Without its list (an older hub, a failed read) a picker is a text box. */
export type PickerType =
  | "thread"
  | "runtime"
  | "box"
  | "account"
  | "permission-mode"
  | "speaker"
  | "voice"
  | "job"
  | "trash-item"
  | "watch"
  | "device"
  | "memory-note"
  | "held"
  | "scopes";
export interface Command {
  id: string;
  title: string;
  description: string;
  group: string;
  method?: string;
  fields: Field[];
  confirm?: string;
  /** A confirmation only some values ask for: `field`'s value `value` (see `confirmationOf`). */
  confirmWhen?: {
    field: string;
    value: string;
    title: string;
    detail: string;
    action: string;
  };
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
const thread = f("thread", "Thread", { type: "thread", required: true });
const runtime = f("runtime", "Runtime", {
  type: "runtime",
  placeholder: "claude-code, codex, grok-build, or a configured runtime",
  required: true,
});
const account = f("account", "Account", { type: "account", required: true });
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
      f("thread", "Thread", { type: "thread", empty: "Every thread" }),
      f("before", "Before event ID", { advanced: true }),
      f("limit", "Results (1–100)", {
        type: "number",
        initial: 30,
        advanced: true,
      }),
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
      f("before", "Earlier events (0–30)", {
        type: "number",
        initial: 12,
        advanced: true,
      }),
      f("after", "Later events (0–30)", {
        type: "number",
        initial: 12,
        advanced: true,
      }),
    ],
    "Read the selected event and its surrounding history without changing the message destination.",
  ),
  cmd("device-list", "Paired devices", "Devices", "device.list"),
  cmd(
    "device-revoke",
    "Revoke device access",
    "Devices",
    "device.revoke",
    [f("id", "Device", { type: "device", required: true })],
    "Disconnect this device from your hubs.",
    "Revoke this device's access?",
  ),
  cmd("device-pair", "Pair a mobile device", "Devices", "device.pair_begin", [
    f("scopes", "Device access", {
      type: "scopes",
      required: true,
      initial: "read operate",
      description: "Give terminals or settings only to a device you trust.",
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
      f("prompt", "First prompt", {
        type: "textarea",
        placeholder: "What the thread should do first (optional)",
        description:
          "Sent to the thread once it exists. With Run as an implementation loop on, it is the loop's task, and required.",
      }),
      f("implement", "Run as an implementation loop", {
        type: "boolean",
        switch: true,
      }),
      f("loop_setup", "Show the loop's setup card first", {
        type: "boolean",
        advanced: true,
        description:
          "Check or change the models before the loop starts, even with saved settings.",
      }),
      {
        ...runtime,
        required: false,
        advanced: true,
        empty: "Default runtime",
      },
      f("box", "Box", { type: "box", advanced: true, empty: "Default box" }),
      f("model", "Model", { type: "model", advanced: true }),
      f("effort", "Reasoning effort", { type: "effort", advanced: true }),
      f("mode", "Permission mode", {
        type: "permission-mode",
        advanced: true,
        empty: "Default mode",
      }),
      f("account", "Account or account pool", {
        type: "account",
        advanced: true,
        empty: "The runtime's default",
      }),
      f("aliases", "Aliases", { type: "words", advanced: true }),
      f("base", "Base branch or commit", { advanced: true }),
      f("workspace_mode", "Checkout", {
        type: "select",
        advanced: true,
        options: ["default", "separate", "shared"],
        choices: [
          { value: "default", label: "The project's default" },
          { value: "separate", label: "Its own worktree" },
          { value: "shared", label: "The project's checkout, shared" },
        ],
        initial: "default",
      }),
      f("outside_pin", "Allow an account outside project pins", {
        type: "boolean",
        advanced: true,
      }),
    ],
    "Give an agent its own thread and working directory.",
  ),
  cmd("mode", "Change permission mode", "Threads", "thread.set_mode", [
    thread,
    f("mode", "Permission mode", { required: true, type: "permission-mode" }),
  ]),
  cmd(
    "model",
    "Change model, runtime or account",
    "Threads",
    "thread.set_model",
    [
      thread,
      f("runtime", "Runtime", { type: "runtime", keep: true }),
      f("model", "Model", { type: "model", keep: true }),
      f("effort", "Reasoning effort", { type: "effort", keep: true }),
      f("account", "Account or pool", {
        type: "account",
        keep: true,
        advanced: true,
      }),
      f("outside_pin", "Allow outside account pins", {
        type: "boolean",
        advanced: true,
      }),
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
      f("force", "Interrupt running work", { type: "boolean", advanced: true }),
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
      choices: [
        { value: "on", label: "On" },
        { value: "off", label: "Off" },
      ],
      advanced: true,
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
    [f("id", "Held message", { type: "held", required: true })],
  ),
  cmd("orch", "Switch orchestrator", "Orchestrator", "orchestrator.use", [
    runtime,
    f("model", "Model", { type: "model" }),
    f("summary", "Include a handoff summary", {
      type: "boolean",
      initial: true,
    }),
    f("wait_secs", "Wait for running turn (seconds)", {
      type: "number",
      initial: 60,
      advanced: true,
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
  {
    ...cmd(
      "access",
      "Orchestrator access",
      "Orchestrator",
      "orchestrator.access",
      [
        f("access", "Access", {
          type: "select",
          required: true,
          options: ACCESS_CHOICES.map((choice) => choice.value),
          choices: ACCESS_CHOICES.map(({ value, label }) => ({ value, label })),
          initial: "limited",
          description: ACCESS_CHOICES.map(
            (choice) => `${choice.label}: ${choice.description}`,
          ).join(" "),
        }),
      ],
      "What the orchestrator may do with its own tools. Full asks first.",
    ),
    confirmWhen: { field: "access", value: "full", ...FULL_ACCESS_CONFIRM },
  },
  cmd("watch", "Watch a thread", "Orchestrator", "orchestrator.watch", [
    thread,
    f("note", "What should the orchestrator do?", {
      required: true,
      type: "textarea",
    }),
    f("repeat", "Keep watching", { type: "boolean" }),
  ]),
  cmd("unwatch", "Remove a watch", "Orchestrator", "orchestrator.unwatch", [
    f("watch", "Watch", { type: "watch", required: true }),
  ]),
  cmd("watches", "List watches", "Orchestrator", "orchestrator.watches"),
  cmd("account-check", "Check account login", "Accounts", "account.status", [
    f("runtime", "Runtime", { type: "runtime", empty: "Every runtime" }),
    f("account", "Account", { type: "account", empty: "Every account" }),
    f("refresh", "Refresh login state", { type: "boolean", initial: true }),
  ]),
  cmd("account-native", "Name a native account", "Accounts", "account.add", [
    runtime,
    f("account", "Account name", { required: true }),
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
      f("move_to", "Move its threads to account", {
        type: "account",
        empty: "Don't move them",
      }),
      f("force", "Remove while threads still reference it", {
        type: "boolean",
        advanced: true,
      }),
    ],
    "Login files are kept. Purge and logout are available under CLI actions.",
    "Remove this account from twr?",
  ),
  cmd("box-refresh", "Refresh installed agents", "Boxes", "box.agents", [
    f("box", "Box", { type: "box", required: true }),
    f("refresh", "Probe installed agents", { type: "boolean", initial: true }),
  ]),
  cmd("box-projects", "List box projects", "Boxes", "box.projects", [
    f("box", "Box", { type: "box", required: true }),
  ]),
  cmd("job", "Run a job", "Jobs", "job.start", [
    thread,
    f("argv", "Command and arguments", {
      required: true,
      type: "words",
      placeholder: "cargo test --workspace",
    }),
    f("name", "Job name"),
    f("slots", "Job slots", { type: "number", initial: 1, advanced: true }),
    f("timeout_secs", "Timeout (seconds)", { type: "number", advanced: true }),
    f("wake", "Tell the thread when it finishes", {
      type: "boolean",
      initial: true,
    }),
  ]),
  cmd("job-log", "Read job output", "Jobs", "job.log", [
    f("job", "Job", { type: "job", required: true }),
    f("lines", "Lines", { type: "number", initial: 200, advanced: true }),
  ]),
  cmd(
    "job-cancel",
    "Cancel job",
    "Jobs",
    "job.cancel",
    [f("job", "Job", { type: "job", required: true })],
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
    f("name", "Note", { type: "memory-note", required: true }),
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
    f("rev", "Existing revision (when editing)", { advanced: true }),
  ]),
  cmd(
    "memory-forget",
    "Forget a memory note",
    "Context & memory",
    "memory.forget",
    [
      f("name", "Note", { type: "memory-note", required: true }),
      f("rev", "Revision", { advanced: true }),
    ],
    "Remove its file from the orchestrator’s memory.",
    "Forget this note?",
  ),
  cmd("restore", "Restore from trash", "Trash", "trash.restore", [
    f("item", "Thread", { type: "trash-item", required: true }),
    f("as", "Restore with a different name", { advanced: true }),
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
      f("since", "Period", {
        type: "select",
        options: ["1d", "7d", "30d"],
        choices: SINCE_CHOICES,
        empty: "All time",
      }),
      f("by", "Group by", {
        type: "select",
        options: ["thread", "project", "runtime"],
        initial: "thread",
      }),
    ],
  ),
  cmd("read", "Read messages aloud", "Speech", "speech.request", [
    f("thread", "Thread", { type: "thread", empty: "The orchestrator" }),
    f("last", "Last messages", { type: "number", initial: 1 }),
    f("speaker", "Speaker", { type: "speaker", empty: "The default speaker" }),
    f("seq", "Specific message sequence (instead of last messages)", {
      type: "number",
      advanced: true,
    }),
  ]),
  cmd("speech-control", "Control reading aloud", "Speech", "speech.control", [
    f("op", "Action", {
      type: "select",
      options: ["stop", "pause", "resume", "skip", "live", "voice", "rate"],
      choices: [
        { value: "stop", label: "Stop reading" },
        { value: "pause", label: "Pause" },
        { value: "resume", label: "Resume" },
        { value: "skip", label: "Skip this message" },
        { value: "live", label: "Read new messages on or off" },
        { value: "voice", label: "Change the voice" },
        { value: "rate", label: "Change the speed" },
      ],
      required: true,
    }),
    f("speaker", "Speaker", { type: "speaker", empty: "Every speaker" }),
    f("voice", "Voice", { type: "voice", empty: "Keep the voice" }),
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
    f("since_seq", "After sequence", {
      type: "number",
      initial: 0,
      advanced: true,
    }),
    f("limit", "Page size", { type: "number", initial: 200, advanced: true }),
  ]),
  cmd("control", "Read control log", "Diagnostics", "control.read", [
    f("since", "After global sequence", {
      type: "number",
      initial: 0,
      advanced: true,
    }),
    f("limit", "Page size", { type: "number", initial: 200, advanced: true }),
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
    let value = values[field.key];
    // A custom model id is typed: spaces around it are no part of it.
    if (field.type === "model" && typeof value === "string")
      value = value.trim();
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
        : field.type === "words" || field.type === "scopes"
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
/** What running `command` with `values` asks first, or nothing: its `confirm`, or its
 * `confirmWhen` when the form holds that value. */
export function confirmationOf(
  command: Command,
  values: Record<string, any>,
): { title: string; detail: string; action?: string } | undefined {
  const when = command.confirmWhen;
  if (when && values[when.field] === when.value)
    return { title: when.title, detail: when.detail, action: when.action };
  if (command.confirm) return { title: command.title, detail: command.confirm };
  return undefined;
}
