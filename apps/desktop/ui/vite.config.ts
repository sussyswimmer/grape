import { resolve } from "node:path";
import { defineConfig } from "vitest/config";

// One HTML entry per Tauri window. The dev server port must match `devUrl`
// in src-tauri/tauri.conf.json, so a busy port is an error, not a retry.
export default defineConfig({
  clearScreen: false,
  server: {
    host: "localhost",
    port: 1420,
    strictPort: true,
  },
  build: {
    // WebView2 on Windows and WKWebView on macOS 13+ both handle ES2022.
    target: "es2022",
    rolldownOptions: {
      input: {
        overlay: resolve(import.meta.dirname, "overlay.html"),
        settings: resolve(import.meta.dirname, "settings.html"),
        toast: resolve(import.meta.dirname, "toast.html"),
        float: resolve(import.meta.dirname, "float.html"),
        panel: resolve(import.meta.dirname, "panel.html"),
      },
    },
  },
  test: {
    include: ["src/**/*.test.ts"],
    // Vitest blanks every stylesheet by default. Let `?raw` imports through
    // so src/styles/palette.test.ts can read the CSS text; plain CSS imports
    // stay blank.
    css: { include: [/\.css\?raw(?:&|$)/] },
  },
});
