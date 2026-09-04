import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The console is served by Vite on its own; every /api call is proxied to the control plane.
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5180,
    proxy: {
      "/api": { target: "http://127.0.0.1:4720", changeOrigin: true },
    },
  },
});
