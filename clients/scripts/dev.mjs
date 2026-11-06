import { spawn } from "node:child_process";
import { build } from "esbuild";
import { createServer } from "vite";
await import("./build-host.mjs");
const server = await createServer();
await server.listen();
const child = spawn("node_modules/.bin/electron", ["."], {
  stdio: "inherit",
  env: { ...process.env, TWR_DESKTOP_DEV_URL: server.resolvedUrls.local[0] },
});
child.on("exit", async (code) => {
  await server.close();
  process.exit(code || 0);
});
for (const signal of ["SIGINT", "SIGTERM"])
  process.on(signal, () => child.kill(signal));
