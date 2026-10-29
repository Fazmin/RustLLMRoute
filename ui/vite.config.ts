import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

const proxied = [
  '/key',
  '/team',
  '/user',
  '/spend',
  '/global',
  '/model',
  '/model_group',
  '/health',
  '/config',
  '/login',
  '/logout',
  '/v1',
  '/metrics',
]

export default defineConfig({
  base: '/ui/',
  plugins: [react(), tailwindcss()],
  server: {
    proxy: Object.fromEntries(proxied.map((path) => [path, 'http://localhost:4000'])),
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
  },
})
