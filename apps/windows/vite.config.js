import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

const page = (path) => fileURLToPath(new URL(path, import.meta.url));

export default defineConfig(({ mode }) => ({
  root: 'ui',
  base: './',
  plugins: [svelte()],
  clearScreen: false,
  // `npm run dev:mock` serves the window in a plain browser for screenshots:
  // Tauri's IPC is swapped for ui/mock, picked by `?screen=`. Only the mock
  // mode resolves these aliases, so no build ever contains the mock.
  resolve:
    mode === 'mock'
      ? {
          alias: [
            { find: /^@tauri-apps\/api\/core$/, replacement: page('./ui/mock/core.js') },
            { find: /^@tauri-apps\/api\/event$/, replacement: page('./ui/mock/event.js') },
          ],
        }
      : {},
  server: mode === 'mock' ? { port: 1430 } : { port: 1420, strictPort: true },
  build: {
    outDir: '../dist',
    emptyOutDir: true,
    // Two pages: the settings window and the recording Overlay.
    rollupOptions: {
      input: {
        main: page('./ui/index.html'),
        overlay: page('./ui/overlay/overlay.html'),
      },
    },
  },
}));
