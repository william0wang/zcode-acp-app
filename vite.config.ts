import { readFileSync } from "node:fs";
import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

import { cloudflare } from "@cloudflare/vite-plugin";

// Single version source is package.json (release.sh bumps it); injected as
// the __APP_VERSION__ constant for the UI to display.
const pkg = JSON.parse(
  readFileSync(new URL("./package.json", import.meta.url), "utf8"),
) as { version: string };

// The cloudflare plugin redirects vite build output to .cloudflare/output and
// stops writing (and emptying) dist/. Tauri embeds dist/ into the Android
// library at compile time, so plain builds must keep using it — only cf deploy
// (deploy-web.sh exports WEB_DEPLOY_BUILD=1) gets the worker build.
const webDeployBuild = process.env.WEB_DEPLOY_BUILD === "1";

export default defineConfig({
  plugins: [react(), tailwindcss(), ...(webDeployBuild ? [cloudflare()] : [])],
  clearScreen: false,
  define: {
    __APP_VERSION__: JSON.stringify(pkg.version),
  },
  server: {
    host: true, // reachable from adb devices during `tauri android dev`
    port: 5173,
    strictPort: true,
  },
  test: {
    // Defaults minus nothing, plus the agent scratch dir: research clones
    // under .zcode/ carry their own test suites that vitest would swallow.
    exclude: ["**/node_modules/**", "**/dist/**", ".zcode/**"],
  },
});
