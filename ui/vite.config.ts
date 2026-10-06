import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The bundle is embedded into the `anchor` binary (crates/anchor-backend
// build.rs) and served under a strict CSP: no inline scripts or styles,
// so no module-preload polyfill and no source maps.
export default defineConfig({
  plugins: [react()],
  base: "/",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    assetsDir: "assets",
    sourcemap: false,
    modulePreload: { polyfill: false },
    assetsInlineLimit: 0,
    cssCodeSplit: false,
  },
});
