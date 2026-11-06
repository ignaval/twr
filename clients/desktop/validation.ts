import { z } from "zod";
export const workspaceSchema = z
  .string()
  .regex(/^(?:[a-z0-9]|[a-z0-9][a-z0-9-]{0,30}[a-z0-9])$/);
export const argsSchema = z
  .array(
    z
      .string()
      .max(65536)
      .refine((v) => !v.includes("\0")),
  )
  .max(256);
const roots = new Set([
  "hub",
  "update",
  "info",
  "usage",
  "thread",
  "orchestrator",
  "say",
  "send",
  "wait",
  "attach",
  "respond",
  "attention",
  "mode",
  "model",
  "rename",
  "stop",
  "park",
  "trim",
  "resume",
  "compact",
  "delete",
  "trash",
  "log",
  "job",
  "speech",
  "speak",
  "remote",
  "events",
  "context",
  "memory",
  "control",
  "terminal-setup",
  "ws",
  "box",
  "boxes",
  "account",
  "runner",
  "--help",
  "--version",
]);
export function cliArgs(input: unknown): string[] {
  const args = argsSchema.parse(input);
  if (!args.length || !roots.has(args[0]))
    throw new Error("Choose a supported twr command");
  const boundaries = [
    args.indexOf("--"),
    args[0] === "box" && args[1] === "add"
      ? args.findIndex(
          (arg) => arg === "--command" || arg.startsWith("--command="),
        )
      : -1,
  ].filter((index) => index >= 0);
  const separator = boundaries.length ? Math.min(...boundaries) : -1;
  if (
    args
      .slice(0, separator < 0 ? args.length : separator)
      .some((arg) => /^(?:--socket|--workspace)(?:=|$)|^-w/.test(arg))
  )
    throw new Error(
      "Commands stay in the selected workspace; socket and workspace overrides are not accepted",
    );
  if (args[0] === "update")
    throw new Error(
      "Update the desktop application using its installer. Its bundled engine must stay matched to the application.",
    );
  if (args[0] === "hub" && args[1] !== "stop")
    throw new Error(
      "The app starts the independent hub when connecting. A foreground hub cannot run inside an app terminal.",
    );
  return args;
}
export function nonInteractiveArgs(input: unknown): string[] {
  const args = cliArgs(input);
  if (!(args[0] === "ws" && ["list", "new"].includes(args[1])))
    throw new Error("Run interactive CLI workflows in an embedded terminal");
  return args;
}
export function workspaceArgs(args: string[], workspace: string): string[] {
  const global =
    args[0] === "runner" ||
    args[0] === "remote" ||
    (["speak", "speech"].includes(args[0]) &&
      args.includes("--all-workspaces"));
  return [...(global ? [] : ["-w", workspace]), ...args];
}
export function externalUrl(input: unknown): string {
  const url = new URL(z.string().max(8192).parse(input));
  if (!["https:", "http:", "mailto:"].includes(url.protocol))
    throw new Error("Only web and email links may open externally");
  return url.href;
}
export const terminalSchema = z
  .object({
    args: argsSchema.optional(),
    cwd: z.string().min(1).max(4096).optional(),
    title: z.string().max(120).optional(),
    shell: z.boolean().optional(),
  })
  .strict();
export const sizeSchema = z.object({
  cols: z.number().int().min(2).max(500),
  rows: z.number().int().min(1).max(300),
});
