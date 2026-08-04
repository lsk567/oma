import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

/**
 * The browser-entered build of the same application, for `omar serve --ui`.
 *
 * `vite.config.ts` builds the Worker: server-rendered, deployed, and told the
 * daemon's address at launch. This one is a static bundle the daemon itself
 * hands out, so it is entered from `spa/index.html` and reads the address from
 * the page it was served by.
 *
 * Output names are fixed rather than hashed, and dynamic imports are inlined,
 * so what lands in `dist/spa` is a known list of files the runtime can embed
 * without a manifest. Cache-busting buys nothing on loopback.
 */
export default defineConfig({
  root: "spa",
  // The logo and favicon live with the Worker build; both entries serve them
  // from the same place.
  publicDir: "../public",
  plugins: [react()],
  build: {
    outDir: "../dist/spa",
    emptyOutDir: true,
    rollupOptions: {
      output: {
        codeSplitting: false,
        entryFileNames: "app.js",
        assetFileNames: "app.[ext]",
      },
    },
  },
});
