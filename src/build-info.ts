import { version as APP_VERSION } from '../package.json';

interface BuildInfo { label: string; commit: string | null; tag: string | null; }
declare const __APP_BUILD_INFO__: BuildInfo;

// Vite embeds this data in the app; tests outside Vite use a development label.
export const APP_BUILD_INFO: BuildInfo = typeof __APP_BUILD_INFO__ === 'undefined'
  ? { label: 'dev', commit: null, tag: null } : __APP_BUILD_INFO__;
export const APP_BUILD_LABEL = APP_BUILD_INFO.label;
export const APP_BUILD_TOOLTIP = `Version ${APP_VERSION}${APP_BUILD_INFO.commit ? ` · Commit ${APP_BUILD_INFO.commit}` : ''}`;
export { APP_VERSION };
