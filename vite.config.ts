import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "node:path";

// Tauri drives the renderer on a fixed port; failing loudly beats silently
// serving on a port the Rust side is not pointing at.
export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: { "@": path.resolve(__dirname, "./src") },
  },
  clearScreen: false,
  server: {
    port: 3100,
    strictPort: true,
    host: "127.0.0.1",
    watch: {
      ignored: [
        "**/src-tauri/**",
        "**/target/**",
        "**/dist/**",
        "**/.git/**",
        "**/evals/**",
      ],
    },
  },
  build: {
    target: "chrome110",
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
    minify: process.env.TAURI_ENV_DEBUG ? false : "esbuild",
  },
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.{ts,tsx}", "tests/**/*.test.{ts,tsx}"],
    setupFiles: ["./tests/setup.ts"],
  },
});
