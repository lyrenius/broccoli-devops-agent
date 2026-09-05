import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The console is served by Vite on its own; every /api call is proxied to the control plane.
// Point BROCCOLI_API at a control plane on another address or port (default: localhost:4720).
const api = process.env.BROCCOLI_API ?? "http://127.0.0.1:4720";

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5180,
    proxy: {
      "/api": { target: api, changeOrigin: true },
    },
  },
});
