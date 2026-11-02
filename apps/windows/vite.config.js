import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

const page = (path) => fileURLToPath(new URL(path, import.meta.url));

export default defineConfig({
  root: 'ui',
  base: './',
  plugins: [svelte()],
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  build: {
    outDir: '../dist',
    emptyOutDir: true,
    // Two pages: the setup window and the recording Overlay.
    rollupOptions: {
      input: {
        main: page('./ui/index.html'),
        overlay: page('./ui/overlay/overlay.html'),
      },
    },
  },
});
