import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vite';

// Tauri's dev server expectations: fixed port, no auto-open, no clearing the
// terminal (so Rust build errors from `cargo` stay visible alongside Vite's).
// See tauri.conf.json's `build.devUrl` / `build.frontendDist`, which this
// config must agree with.
export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      // Cargo writes/locks files under target/ while compiling; watching
      // them races the linker on Windows and crashes Vite with EBUSY.
      ignored: ['**/target/**', '**/crates/**/target/**'],
    },
  },
  envPrefix: ['VITE_', 'TAURI_'],
  build: {
    outDir: 'dist',
    // The old chrome105/safari13 split (from earlier Tauri templates) is too
    // old for Svelte 5's compiled output (esbuild can't down-transform the
    // destructuring it emits). SP1 only targets Windows, where the WebView2
    // runtime tracks current Chromium, so a single modern target is correct
    // and simpler.
    target: 'es2022',
    minify: !process.env.TAURI_ENV_DEBUG ? 'esbuild' : false,
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
  },
});
