import type { Command, Field } from "../shared/commands";
export function parseHelp(
  path: string[],
  help: string,
): { command: Command; children: string[]; callable: boolean } {
  let section = "";
  const fields: Field[] = [];
  const cliFields: NonNullable<Command["cliFields"]> = [];
  const children: string[] = [];
  const usage = help.match(/^Usage: (.+)$/m)?.[1] || "";
  let last: Field | undefined;
  for (const line of help.split("\n")) {
    if (/^[A-Za-z ]+:$/.test(line)) {
      section = line.slice(0, -1);
      last = undefined;
      continue;
    }
    if (section === "Commands") {
      const match = line.match(/^  ([a-z][a-z-]*)\s{2,}/);
      if (match && match[1] !== "help") children.push(match[1]);
    }
    if (section === "Arguments") {
      const match = line.match(
        /^  ([<[])([A-Z_][A-Z_0-9 -]*)[>\]](\.\.\.)?(?:\s{2,}(.*))?$/,
      );
      if (match) {
        const key = match[2].toLowerCase().replaceAll(" ", "_");
        const separator =
          usage.includes(`-- <${match[2]}>`) ||
          usage.includes(`-- [${match[2]}]`);
        const literal = ["PROMPT", "MESSAGE", "TEXT"].includes(match[2]);
        last = {
          key,
          label: match[2].toLowerCase().replaceAll("_", " "),
          type: literal ? "textarea" : match[3] ? "words" : "text",
          required: match[1] === "<",
          description: match[4] || "",
        };
        fields.push(last);
        cliFields.push({ key, many: !!match[3], separator, literal });
        continue;
      }
    }
    if (section === "Options") {
      const match = line.match(
        /^\s+(?:-[\w?],\s+)?(--[a-z][a-z0-9-]*)(?:[ =]([<[])[A-Z_0-9 -]+[>\]])?(\.\.\.)?(?:\s{2,}(.*))?$/,
      );
      if (match) {
        if (
          ["--help", "--version", "--workspace", "--socket", "--yes"].includes(
            match[1],
          )
        ) {
          last = undefined;
          continue;
        }
        const key = match[1].slice(2);
        const takesValue = !!match[2];
        last = {
          key,
          label: key.replaceAll("-", " "),
          type: !takesValue ? "boolean" : match[3] ? "words" : "text",
          description: match[4] || "",
          advanced: ["json", "pretty", "hydrate"].includes(key),
        };
        fields.push(last);
        cliFields.push({
          key,
          flag: match[1],
          many: !!match[3] || key === "alias",
          separator: !!match[3] && ["command", "argv"].includes(key),
        });
        continue;
      }
    }
    if (last && /^\s{4,}\S/.test(line))
      last.description = (last.description + " " + line.trim()).trim();
  }
  for (const field of fields) {
    const options = field.description
      ?.match(/\[possible values: ([^\]]+)\]/)?.[1]
      .split(", ");
    if (options && field.type === "text") {
      field.type = "select";
      field.options = options;
    }
  }
  return {
    command: {
      id: `cli:${path.join(" ")}`,
      title: `twr ${path.join(" ")}`,
      description: help.split("\n\n")[0],
      group: "CLI · " + (path[0] || "General"),
      fields,
      cli: path,
      cliFields,
    },
    children,
    callable: !children.length || !usage.includes("<COMMAND>"),
  };
}
export async function discoverCatalog(
  help: (args: string[]) => Promise<string>,
): Promise<Command[]> {
  const output: Command[] = [];
  async function visit(path: string[], depth: number) {
    const parsed = parseHelp(path, await help([...path, "--help"]));
    if (path.length && parsed.callable) output.push(parsed.command);
    if (depth < 4)
      for (let i = 0; i < parsed.children.length; i += 4)
        await Promise.all(
          parsed.children
            .slice(i, i + 4)
            .map((child) => visit([...path, child], depth + 1)),
        );
  }
  await visit([], 0);
  return output.sort((a, b) => a.title.localeCompare(b.title));
}
