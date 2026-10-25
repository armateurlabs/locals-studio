import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type { JsonObject, StudioEvent } from './types';

export function isDesktop() {
  return '__TAURI_INTERNALS__' in window;
}

export async function request<T = any>(method: string, params: JsonObject = {}): Promise<T> {
  if (!isDesktop()) throw new Error('Open Locals Studio as a desktop app to connect to your local service. Run npm run desktop.');
  return invoke<T>('studio_request', { method, params });
}

export async function subscribe(callback: (event: StudioEvent) => void): Promise<() => void> {
  if (!isDesktop()) return () => {};
  const unsubscribe = await listen<StudioEvent>('studio:event', event => callback(event.payload));
  try { await invoke('studio_events'); }
  catch (error) { unsubscribe(); throw error; }
  return unsubscribe;
}

export async function openExternal(url: string): Promise<void> {
  const parsed = new URL(url);
  if (parsed.protocol !== 'https:' || !['huggingface.co', 'armateurlabs.fr'].includes(parsed.hostname) || parsed.username || parsed.password) throw new Error('Only HTTPS links to Hugging Face or Armateur Labs can be opened, without URL credentials.');
  if (isDesktop()) return invoke('open_external', { url });
  window.open(url, '_blank', 'noopener,noreferrer');
}

export function errorMessage(error: unknown) {
  if (error instanceof Error) return error.message;
  if (typeof error === 'string') return error;
  return JSON.stringify(error);
}
