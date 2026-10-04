import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri expects a fixed dev port and serves the built assets from ../dist.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: [
        "**/target/**",
        "**/node_modules/**",
      ],
    },
  },
  build: {
    outDir: "dist",
    target: "es2021",
    rollupOptions: {
      output: {
        // Split heavy, independently-used vendor deps into their own chunks so
        // the webview can parse them in parallel and reuse them across rebuilds.
        manualChunks(id) {
          if (!id.includes("node_modules")) return undefined;
          if (id.includes("@xterm")) return "vendor-xterm";
          if (id.includes("@codemirror") || id.includes("/codemirror/")) return "vendor-codemirror";
          if (id.includes("/bson/") || id.includes("mongodb-query-parser")) return "vendor-mongo";
          if (id.includes("/react/") || id.includes("/react-dom/") || id.includes("/scheduler/")) {
            return "vendor-react";
          }
          return "vendor";
        },
      },
    },
  },
});
