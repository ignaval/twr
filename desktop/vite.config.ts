import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
export default defineConfig({
  plugins: [
    react(),
    {
      name: "dev-csp",
      apply: "serve",
      transformIndexHtml(html) {
        return html
          .replace("connect-src 'self'", "connect-src 'self' ws://127.0.0.1:*")
          .replace("script-src 'self'", "script-src 'self' 'unsafe-inline'");
      },
    },
  ],
  base: "./",
  build: { outDir: "dist", emptyOutDir: true },
  server: { host: "127.0.0.1" },
  test: { include: ["tests/**/*.test.ts"], environment: "node" },
} as Parameters<typeof defineConfig>[0]);
