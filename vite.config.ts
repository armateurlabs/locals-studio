import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { resolveBuildInfo } from './scripts/build-info.mjs';

const buildInfo = process.env.LOCALS_STUDIO_BUILD_INFO
  ? JSON.parse(process.env.LOCALS_STUDIO_BUILD_INFO) : resolveBuildInfo();
if (!buildInfo || typeof buildInfo.label !== 'string' || !buildInfo.label
    || (buildInfo.commit !== null && (typeof buildInfo.commit !== 'string' || !/^(?:[a-f\d]{40}|[a-f\d]{64})$/i.test(buildInfo.commit)))
    || (buildInfo.tag !== null && (typeof buildInfo.tag !== 'string' || !buildInfo.tag))) {
  throw new Error('Invalid app build identity. Expected label, full commit SHA (or null), and tag (or null).');
}

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 1420, strictPort: true, host: '127.0.0.1' },
  envPrefix: ['VITE_', 'TAURI_ENV_'],
  define: { __APP_BUILD_INFO__: JSON.stringify(buildInfo) },
  build: { target: 'safari15', sourcemap: false },
});
