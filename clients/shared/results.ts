/** What a command form's result says, in words: a sentence, a small table or list, or a
 * text to read, per RPC method (`resultSummary`). A method without a formatter, or a
 * result its formatter doesn't recognise, shows as data (`DataView`); a formatted one keeps
 * the data under Details. */
import { permissionLabel } from "./labels";
import type { Json, UpdateStatus } from "./types";

export interface ResultSummary {
  /** One or two sentences. */
  text?: string;
  /** Something that went wrong, or needs doing, beside a done result. */
  warning?: string;
  table?: { columns: string[]; rows: string[][] };
  list?: string[];
  /** A text to read (a memory note's body), as Markdown. */
  body?: string;
  /** Offer to open this thread. */
  openThread?: { id: string; name: string };
  /** Offer this link (an update's download or release page). */
  link?: { label: string; url: string };
  /** Offer Restart to update (a downloaded update). */
  restart?: boolean;
  /** Offer the app's update settings. */
  updateSettings?: boolean;
}

type Formatter = (value: Json, values: Json) => ResultSummary | undefined;

const isObject = (value: Json) =>
  !!value && typeof value === "object" && !Array.isArray(value);
const nameOf = (value: Json) =>
  value?.thread_name || value?.name || value?.thread || "The thread";
const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? "" : "s"}`;
const words = (text: unknown) =>
  typeof text === "string" ? text.replaceAll("_", " ") : "";

/** A byte count in words: 1.2 MB. */
export function formatBytes(bytes: unknown): string {
  const n = Number(bytes);
  if (!Number.isFinite(n) || n < 0) return "—";
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = n / 1024,
    unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  return `${value >= 10 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}

/** A change's outcome: `done` when it changed, else the hub's own message. */
function changed(value: Json, done: string): ResultSummary {
  const summary: ResultSummary = {
    text: value.changed === false ? value.message || "Nothing changed." : done,
  };
  const warning = [value.warning, value.dispatch_error]
    .filter((w) => typeof w === "string" && w)
    .join(" ");
  if (warning) summary.warning = warning;
  return summary;
}

/** `account.list` / `account.status`: one row per account. */
function accounts(value: Json): ResultSummary | undefined {
  if (!isObject(value?.runtimes)) return undefined;
  const rows = Object.entries(value.runtimes).flatMap(
    ([runtime, group]: [string, Json]) =>
      (Array.isArray(group?.accounts) ? group.accounts : [])
        .filter((a: Json) => !a?.removed)
        .map((a: Json) => [
          runtime,
          a.native ? `${a.name} (native login)` : String(a.name),
          words(a.login) || "Not checked",
        ]),
  );
  return rows.length
    ? { table: { columns: ["Runtime", "Account", "Login"], rows } }
    : { text: "No accounts yet." };
}

const formatters: Record<string, Formatter> = {
  "thread.new": (value) =>
    value?.id
      ? {
          text: `Created ${value.name || value.id}.`,
          warning:
            typeof value.warning === "string" ? value.warning : undefined,
          openThread: { id: value.id, name: value.name || value.id },
        }
      : undefined,
  "thread.set_mode": (value) => {
    if (!isObject(value)) return undefined;
    if (value.status === "needs_confirmation")
      return {
        text: "The orchestrator's new mode waits for your confirmation on its card.",
      };
    return changed(
      value,
      `${nameOf(value)} now uses ${permissionLabel(value.mode)}.`,
    );
  },
  "thread.set_model": (value) => {
    if (!isObject(value)) return undefined;
    const parts = [
      value.runtime_changed && `the ${value.runtime} runtime`,
      value.model !== value.previous_model &&
        (value.model
          ? `the ${value.model} model`
          : "the runtime's default model"),
      value.effort !== value.previous_effort &&
        (value.effort ? `${value.effort} effort` : "the default effort"),
      value.account_changed &&
        (value.account
          ? `the ${value.account} account`
          : "the default account"),
    ].filter(Boolean);
    return changed(
      value,
      parts.length
        ? `${nameOf(value)} now uses ${listWords(parts as string[])}.`
        : `${nameOf(value)}'s settings changed.`,
    );
  },
  "thread.rename": (value) =>
    isObject(value)
      ? changed(
          value,
          value.previous
            ? `Renamed ${value.previous} to ${nameOf(value)}.`
            : `Renamed to ${nameOf(value)}.`,
        )
      : undefined,
  "thread.park": (value) => {
    if (!isObject(value)) return undefined;
    const kept = value.worktree_removed
      ? " Its worktree was removed; its branch stays."
      : value.kept || value.kept_dirty
        ? ` Its worktree was kept${value.kept_reason ? `: ${value.kept_reason}` : "."}`
        : "";
    return {
      text: `Parked ${nameOf(value)}.${kept}`,
      warning: value.worktree_error || undefined,
    };
  },
  "thread.resume": (value) =>
    isObject(value)
      ? {
          text: `Resumed ${nameOf(value)}.${value.worktree_recreated ? " Its worktree was made again." : ""}`,
        }
      : undefined,
  "thread.stop": (value) => {
    if (!isObject(value)) return undefined;
    const dropped = Number(value.dropped_prompts) || 0;
    return {
      text:
        (value.stopping
          ? `Stopping ${nameOf(value)}.`
          : `${nameOf(value)} wasn't working.`) +
        (dropped ? ` Dropped ${plural(dropped, "queued message")}.` : ""),
      warning: value.box_offline
        ? "Its box is offline: it stops once the box is back."
        : undefined,
    };
  },
  "thread.compact": (value) => {
    if (!isObject(value)) return undefined;
    if (value.offers)
      return {
        text: `Automatic compaction is ${value.offers} for ${nameOf(value)}.`,
      };
    return {
      text: value.already
        ? `${nameOf(value)} is already compacting.`
        : value.state === "queued"
          ? `${nameOf(value)} compacts after its current turn.`
          : `Compacting ${nameOf(value)}.`,
    };
  },
  "thread.trim": (value) => {
    if (!isObject(value)) return undefined;
    const deleted = Array.isArray(value.deleted) ? value.deleted : [];
    return {
      text: deleted.length
        ? `Freed ${formatBytes(value.freed)} from ${nameOf(value)}.`
        : `${nameOf(value)} had no build output to remove.`,
      list: [...deleted, ...(Array.isArray(value.notes) ? value.notes : [])]
        .filter((line) => typeof line === "string")
        .slice(0, 20),
    };
  },
  "account.status": accounts,
  "account.list": accounts,
  "log.stats": (value) => {
    if (!isObject(value?.db)) return undefined;
    const rows: string[][] = [
      ["Event log", formatBytes((value.db.bytes || 0) + (value.db.wal || 0))],
    ];
    if (isObject(value.archive))
      rows.push([
        "Archive",
        formatBytes((value.archive.bytes || 0) + (value.archive.wal || 0)),
      ]);
    if (isObject(value.next) && value.next.rows)
      rows.push([
        "Ready to archive",
        `${formatBytes(value.next.bytes)} in ${plural(value.next.rows, "event")}`,
      ]);
    if (value.retention_days !== undefined)
      rows.push(["Kept for", plural(Number(value.retention_days), "day")]);
    return { table: { columns: ["Storage", "Size"], rows } };
  },
  "usage.threads": (value) => {
    if (!Array.isArray(value?.rows)) return undefined;
    if (!value.rows.length) return { text: "No usage in this period." };
    const by =
      value.by === "project"
        ? "Project"
        : value.by === "runtime"
          ? "Runtime"
          : "Thread";
    const row = (r: Json) => [
      String(r.label ?? r.key),
      tokens(r.input),
      tokens(r.output),
      String(r.turns ?? "—"),
      typeof r.cost === "number" ? `$${r.cost.toFixed(2)}` : "—",
    ];
    return {
      table: {
        columns: [by, "Input", "Output", "Turns", "Cost"],
        rows: [
          ...value.rows.map(row),
          ...(value.total ? [row({ ...value.total, label: "Total" })] : []),
        ],
      },
    };
  },
  "box.agents": (value) => {
    if (!Array.isArray(value?.agents)) return undefined;
    if (!value.agents.length)
      return { text: `No agents found on ${value.box}.` };
    return {
      text: value.online === false ? `${value.box} is offline.` : undefined,
      table: {
        columns: ["Agent", "Installed", "Login"],
        rows: value.agents.map((a: Json) => [
          String(a.runtime),
          a.found ? a.version || "Yes" : "No",
          words(a.login) || "—",
        ]),
      },
    };
  },
  "box.projects": (value) => {
    if (!Array.isArray(value?.projects)) return undefined;
    if (!value.projects.length)
      return { text: `${value.box} has no projects yet.` };
    return {
      list: value.projects.map((p: string) =>
        value.repos?.[p] ? `${p} — ${value.repos[p]}` : p,
      ),
    };
  },
  "desktop.check_updates": (value) => updateSummary(value),
  "memory.read": (value) =>
    typeof value?.body === "string"
      ? {
          text: value.description || undefined,
          body: value.body,
        }
      : undefined,
  "memory.save": (value, values) => ({
    text: `Saved ${value?.name || values?.name || "the note"}.`,
  }),
  "memory.forget": (_value, values) => ({
    text: `Forgot ${values?.name || "the note"}.`,
  }),
  "device.list": (value) =>
    Array.isArray(value)
      ? value.length
        ? {
            table: {
              columns: ["Device", "Access", "Paired"],
              rows: value.map((d: Json) => [
                String(d.name || d.id),
                Array.isArray(d.scopes) ? d.scopes.join(", ") : "—",
                d.created
                  ? new Date(d.created * 1000).toLocaleDateString()
                  : "—",
              ]),
            },
          }
        : { text: "No devices are paired." }
      : undefined,
  "device.revoke": () => ({ text: "The device can no longer connect." }),
  "orchestrator.watches": (value) =>
    Array.isArray(value?.watches)
      ? value.watches.length
        ? {
            table: {
              columns: ["Thread", "Note", "Repeats"],
              rows: value.watches.map((w: Json) => [
                String(w.thread_name || w.thread),
                String(w.note ?? ""),
                w.repeat ? "Yes" : "No",
              ]),
            },
          }
        : { text: "The orchestrator watches nothing." }
      : undefined,
  "orchestrator.unwatch": (value) =>
    Array.isArray(value?.removed)
      ? { text: `Removed ${plural(value.removed.length, "watch")}.` }
      : undefined,
  "thread.send_cancel": (value) =>
    value?.cancelled
      ? { text: `Cancelled the message held for ${nameOf(value)}.` }
      : undefined,
};

function tokens(n: unknown) {
  const value = Number(n);
  if (!Number.isFinite(value)) return "—";
  return new Intl.NumberFormat(undefined, {
    notation: "compact",
    maximumFractionDigits: 1,
  }).format(value);
}

function listWords(parts: string[]) {
  return parts.length < 2
    ? parts.join("")
    : `${parts.slice(0, -1).join(", ")} and ${parts.at(-1)}`;
}

/** An update check in one sentence, with what to do next. */
export function updateSummary(
  status: UpdateStatus | undefined,
): ResultSummary | undefined {
  if (!status || typeof status.state !== "string") return undefined;
  const link = status.download
    ? { label: `Download ${status.download.name}`, url: status.download.url }
    : status.releaseUrl
      ? { label: "Open release page", url: status.releaseUrl }
      : undefined;
  switch (status.state) {
    case "up-to-date":
      return {
        text: `You have the latest version, twr ${status.current}.`,
        updateSettings: true,
      };
    case "available":
      return {
        text: `twr ${status.latest} is available. You have ${status.current}.${!status.automatic && status.instructions ? " " + status.instructions : ""}`,
        link,
        updateSettings: true,
      };
    case "downloading":
      return {
        text: `Downloading twr ${status.latest}… ${status.progress ?? 0}%`,
      };
    case "downloaded":
      return {
        text: `twr ${status.latest} is ready. Restart to update, or it installs the next time you quit.`,
        restart: true,
      };
    case "error":
      return {
        warning: `Could not check for updates: ${status.error || "unknown error"}`,
        link,
        updateSettings: true,
      };
    default:
      return status.kind === "development"
        ? { text: "This is a development build: it doesn't update itself." }
        : {
            text: `You have twr ${status.current}.`,
            link,
            updateSettings: true,
          };
  }
}

/** `method`'s result in words, or undefined to show it as data. */
export function resultSummary(
  method: string | undefined,
  value: Json,
  values: Json = {},
): ResultSummary | undefined {
  const format = method ? formatters[method] : undefined;
  if (!format || value === undefined) return undefined;
  try {
    return format(value, values);
  } catch {
    return undefined;
  }
}
