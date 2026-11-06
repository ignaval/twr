import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { fileURLToPath } from "node:url";
export default defineConfig({
  root: fileURLToPath(new URL(".", import.meta.url)),
  base: "./",
  plugins: [react()],
  build: {
    outDir: "www",
    emptyOutDir: true,
    target: ["safari17.4", "chrome119"],
  },
  server: { host: "127.0.0.1", port: 5174, strictPort: true },
});
