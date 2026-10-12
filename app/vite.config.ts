import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  // Tauri serves the built dist/ directly; no base path rewriting.
  build: { outDir: "dist", emptyOutDir: true, target: "safari15" },
  server: { port: 5173, strictPort: true },
  clearScreen: false,
});
