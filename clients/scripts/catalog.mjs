import { build } from "esbuild";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
const run = promisify(execFile);
export async function generatedCatalog(binary) {
  const result = await build({
    entryPoints: [resolve("desktop/catalog.ts")],
    bundle: true,
    write: false,
    format: "esm",
    platform: "node",
  });
  const { discoverCatalog } = await import(
    `data:text/javascript;base64,${Buffer.from(result.outputFiles[0].text).toString("base64")}`
  );
  return (
    await discoverCatalog(
      async (args) =>
        (
          await run(binary, args, {
            env: { ...process.env, NO_COLOR: "1" },
            maxBuffer: 1048576,
          })
        ).stdout,
    )
  ).filter(
    (item) =>
      !["update", "mcp", "completions"].includes(item.cli?.[0]) &&
      !["hub", "voice transcribe"].includes(item.cli?.join(" ")),
  );
}
if (
  process.argv[1] &&
  import.meta.url === new URL(`file://${resolve(process.argv[1])}`).href
) {
  const binary = resolve(
    process.env.TWR_CATALOG_BINARY || "../target/debug/twr",
  );
  const generated =
    JSON.stringify(await generatedCatalog(binary), null, 2) + "\n";
  const path = "mobile/catalog.json";
  if (process.argv.includes("--check")) {
    if (readFileSync(path, "utf8") !== generated)
      throw new Error(
        "Mobile CLI catalog is stale. Run npm run catalog:update after rebuilding twr.",
      );
    console.log(
      "PASS: mobile catalog matches every current CLI help command and option",
    );
  } else writeFileSync(path, generated);
}
