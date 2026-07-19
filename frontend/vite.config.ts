import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The Rust server (see api.rs) serves static files from "./static" relative
// to the crate root. Building straight into that folder means `cargo run`
// always serves whatever you last built here — no separate deploy step.
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: "../static",
    emptyOutDir: true,
  },
  server: {
    proxy: {
      "/status": "http://127.0.0.1:3219",
      "/config": "http://127.0.0.1:3219",
      "/devices": "http://127.0.0.1:3219",
      "/discord": "http://127.0.0.1:3219",
      "/controller": "http://127.0.0.1:3219",
      "/listeners": "http://127.0.0.1:3219",
      "/quit": "http://127.0.0.1:3219",
      "/ws": {
        target: "ws://127.0.0.1:3219",
        ws: true,
      },
    },
  },
});
