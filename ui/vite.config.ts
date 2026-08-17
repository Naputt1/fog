import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { tanstackRouter } from "@tanstack/router-plugin/vite";
import path from "node:path";

// https://vite.dev/config/
export default defineConfig({
  // The app is served from the Rust hyper server at the root of
  // http://127.0.0.1:18080/. Assets are embedded at build time, so the
  // build must use a root-absolute base so paths resolve wherever the
  // server is mounted. See also `build.target` below (fine for modern
  // browsers / embedded wasm-era frontends).
  base: "/",
  plugins: [
    // File-based routing: routes live in src/routes and the plugin
    // generates src/routeTree.gen.ts automatically.
    tanstackRouter({
      target: "react",
      routesDirectory: "src/routes",
      generatedRouteTree: "src/routeTree.gen.ts",
      // Vite 8 defaults to "html" which would drop the doc type / root id.
      quoteStyle: "single",
    }),
    react(),
    tailwindcss(),
  ],
  resolve: {
    alias: {
      "@": path.resolve(import.meta.dirname, "./src"),
    },
  },
  server: {
    port: 5173,
    // In dev the frontend runs on :5173 while the Rust server exposes the API
    // on :18080. Proxy the API + SSE stream so the client can use relative
    // paths consistently in dev and production.
    proxy: {
      "/api": {
        target: "http://127.0.0.1:18080",
        changeOrigin: true,
      },
      "/logs/stream": {
        target: "http://127.0.0.1:18080",
        changeOrigin: true,
      },
    },
  },
});
