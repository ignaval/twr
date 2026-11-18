/* The orchestrator's access level (`[orchestrator] access`, `orchestrator.access`): the
 * words Settings › Orchestrator and the command palette's Orchestrator access share. The hub
 * decides and applies it; this file only says what it means and what the hub answered. */
import type { Json } from "./types";

export type OrchestratorAccess = "limited" | "full";

/** The level a hub reports (`hub.info`/`orchestrator.show` `access`), or undefined for a
 * hub too old to have one: then nothing offers to change it. */
export function accessLevel(value: unknown): OrchestratorAccess | undefined {
  return value === "limited" || value === "full" ? value : undefined;
}

export const ACCESS_CHOICES: {
  value: OrchestratorAccess;
  label: string;
  description: string;
}[] = [
  {
    value: "limited",
    label: "Limited",
    description:
      "twr's tools and a web search only: no shell, no files, none of your MCP servers.",
  },
  {
    value: "full",
    label: "Full",
    description:
      "Like a worker in bypassPermissions: any command, your files, the web and your MCP servers, with nothing asking first.",
  },
];

/** The confirmation choosing Full asks for (Limited asks nothing). */
export const FULL_ACCESS_CONFIRM = {
  title: "Give the orchestrator full access?",
  detail:
    "It will run like a worker in bypassPermissions: any command, your files, the web and your own MCP servers, with nothing asking first. That holds in every turn, including turns a watch or a finished worker starts while you may be away. It reads more text you didn't write than any other agent (workers' reports, web pages, repository files), so if that text misleads it, it can do anything your account can. It takes effect when the orchestrator's current turn ends.",
  action: "Give full access",
};

/** What `orchestrator.access {access}` did, in words: `text` when it applies, `warning`
 * when config.toml wasn't updated (it lasts until the hub restarts). */
export function accessOutcome(result: Json): {
  text: string;
  warning?: string;
} {
  const level = accessLevel(result?.access);
  const words = level ? `${level} access` : "the new access level";
  const text =
    result?.restart === "now"
      ? `The orchestrator restarted with ${words}. Its conversation continues.`
      : result?.restart === "after_turn"
        ? `The orchestrator is in a turn. It switches to ${words} when that turn ends.`
        : level && result?.previous === level
          ? `The orchestrator already has ${words}.`
          : `No orchestrator is running: the next one starts with ${words}.`;
  if (result?.persisted !== false) return { text };
  const reason = result?.persist_error ? ` (${result.persist_error})` : "";
  return {
    text,
    warning: `config.toml couldn't be updated${reason}, so this applies only until the hub restarts.`,
  };
}

/** A hub from before `orchestrator.access`. */
export function unknownMethod(error: unknown) {
  return /unknown method/i.test(
    error instanceof Error ? error.message : String(error),
  );
}
