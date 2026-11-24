import { build } from "esbuild";
// Typed pairing's Argon2id runs in a worker thread. Its bundle is inlined as source and
// started with `eval`, so no worker file has to be found inside the asar archive.
const worker = await build({
  entryPoints: ["desktop/argon2-worker.ts"],
  bundle: true,
  platform: "node",
  format: "cjs",
  target: "node24",
  write: false,
});
await build({
  entryPoints: { main: "desktop/main.ts", preload: "desktop/preload.ts" },
  outdir: "dist-host",
  bundle: true,
  platform: "node",
  format: "cjs",
  target: "node24",
  outExtension: { ".js": ".cjs" },
  external: ["electron", "node-pty"],
  define: {
    __TWR_ARGON2_WORKER__: JSON.stringify(worker.outputFiles[0].text),
  },
});
