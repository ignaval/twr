import { build } from "esbuild";
await build({
  entryPoints: { main: "electron/main.ts", preload: "electron/preload.ts" },
  outdir: "dist-host",
  bundle: true,
  platform: "node",
  format: "cjs",
  target: "node24",
  outExtension: { ".js": ".cjs" },
  external: ["electron", "node-pty"],
});
