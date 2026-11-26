import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
  server: { host: "localhost", port: 5173, strictPort: true },
  build: { target: "safari16", rollupOptions: { input: ["index.html", "settings.html", "drop.html"] } },
});
