import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const daemon = process.env.AGENT_TRACKER_URL ?? "http://127.0.0.1:4318";

export default defineConfig({
  plugins: [react()],
  // The Connect guide imports its snippets from ../integrations.
  server: { proxy: { "/api": daemon, "/v1": daemon }, fs: { allow: [".."] } },
});
