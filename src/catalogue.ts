import type { JsonObject } from './types';
import { modelParameters } from './model-parameters';
import { modelMetadata } from './utils.mjs';

export type CatalogueSort = 'downloads' | 'parameters' | 'bits' | 'disk';
export type CatalogueDirection = 'asc' | 'desc';
export interface DiscoverPreferences {
  query: string;
  view: 'grid' | 'list';
  sort: CatalogueSort;
  direction: CatalogueDirection;
}
interface CatalogueEntry { results: JsonObject[]; updatedAt: number; }
interface CatalogueState {
  entries: Map<string, CatalogueEntry>;
  preferences: DiscoverPreferences;
}
type CatalogueStorage = Pick<Storage, 'getItem' | 'setItem'>;

const CACHE_KEY = 'studio.discover.catalogue.v1';
const PREFERENCES_KEY = 'studio.discover.preferences';
const MAX_QUERIES = 8;
// localStorage uses UTF-16: this leaves the catalogue below roughly 1 MiB.
const MAX_CACHE_CHARACTERS = 500_000;
const MAX_QUERY_CHARACTERS = 2_048;
const states = new WeakMap<CatalogueStorage, CatalogueState>();
let memoryState: CatalogueState | undefined;

function record(value: unknown): value is JsonObject {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}
function queryKey(query: string): string | undefined {
  if (typeof query !== 'string' || query.length > MAX_QUERY_CHARACTERS) return undefined;
  const normalized = query.trim().toLowerCase();
  return normalized.length <= MAX_QUERY_CHARACTERS ? normalized : undefined;
}
function storage(): CatalogueStorage | undefined {
  try {
    const value = globalThis.localStorage;
    return value && typeof value.getItem === 'function' && typeof value.setItem === 'function' ? value : undefined;
  } catch { return undefined; }
}
function readStored(target: CatalogueStorage | undefined, key: string, limit: number): unknown {
  try {
    const text = target?.getItem(key);
    return text && text.length <= limit ? JSON.parse(text) : undefined;
  } catch { return undefined; }
}
function validResults(value: unknown): value is JsonObject[] {
  return Array.isArray(value) && value.every(item => {
    if (!record(item) || typeof (item.id ?? item.modelId) !== 'string' || !(item.id ?? item.modelId).trim()) return false;
    if (item.tags != null && (!Array.isArray(item.tags) || !item.tags.every((tag: unknown) => typeof tag === 'string'))) return false;
    if (item.siblings != null && (!Array.isArray(item.siblings) || !item.siblings.every(record))) return false;
    return (item.cardData == null || record(item.cardData)) && (item.studio_size == null || record(item.studio_size));
  });
}
function preferences(value: unknown): DiscoverPreferences {
  const input = record(value) ? value : {};
  return {
    query: typeof input.query === 'string' && input.query.length <= MAX_QUERY_CHARACTERS ? input.query : '',
    view: input.view === 'list' ? 'list' : 'grid',
    sort: input.sort === 'weights' ? 'parameters' : ['downloads', 'parameters', 'bits', 'disk'].includes(input.sort) ? input.sort : 'downloads',
    direction: input.direction === 'asc' ? 'asc' : 'desc',
  };
}
function serialized(entries: Map<string, CatalogueEntry>): string {
  return JSON.stringify({ version: 1, entries: [...entries].map(([query, entry]) => ({ query, ...entry })) });
}
function stateFor(target: CatalogueStorage | undefined): CatalogueState {
  const existing = target ? states.get(target) : memoryState;
  if (existing) return existing;
  const state: CatalogueState = {
    entries: new Map(),
    preferences: preferences(readStored(target, PREFERENCES_KEY, MAX_QUERY_CHARACTERS * 6 + 256)),
  };
  const saved = readStored(target, CACHE_KEY, MAX_CACHE_CHARACTERS);
  if (record(saved) && saved.version === 1 && Array.isArray(saved.entries)) {
    for (const entry of saved.entries) {
      if (!record(entry) || !Number.isFinite(entry.updatedAt) || entry.updatedAt < 0 || entry.updatedAt > Date.now() + 60_000 || !validResults(entry.results)) continue;
      const query = queryKey(entry.query);
      if (query === undefined) continue;
      state.entries.delete(query);
      state.entries.set(query, { results: entry.results, updatedAt: entry.updatedAt });
      while (state.entries.size > MAX_QUERIES) state.entries.delete(state.entries.keys().next().value!);
    }
  }
  if (target) states.set(target, state);
  else memoryState = state;
  return state;
}

// Stale entries are returned too, so callers can display them while refreshing.
export function readCatalogue(query: string): CatalogueEntry | undefined {
  const key = queryKey(query);
  if (key === undefined) return undefined;
  const entry = stateFor(storage()).entries.get(key);
  return entry ? { updatedAt: entry.updatedAt, results: JSON.parse(JSON.stringify(entry.results)) } : undefined;
}

export function writeCatalogue(query: string, results: JsonObject[], updatedAt = Date.now()): void {
  const key = queryKey(query);
  if (key === undefined || !Number.isFinite(updatedAt) || updatedAt < 0 || updatedAt > Date.now() + 60_000) return;
  try {
    // Snapshot JSON data so later view enrichment cannot mutate cached results.
    const text = JSON.stringify(results);
    if (!text || text.length > MAX_CACHE_CHARACTERS) return;
    const snapshot: unknown = JSON.parse(text);
    if (!validResults(snapshot)) return;
    const target = storage();
    const state = stateFor(target);
    const entry = { results: snapshot, updatedAt };
    const single = new Map([[key, entry]]);
    if (serialized(single).length > MAX_CACHE_CHARACTERS) return;
    state.entries.delete(key);
    state.entries.set(key, entry);
    let payload = serialized(state.entries);
    while (state.entries.size > MAX_QUERIES || payload.length > MAX_CACHE_CHARACTERS) {
      state.entries.delete(state.entries.keys().next().value!);
      payload = serialized(state.entries);
    }
    try { target?.setItem(CACHE_KEY, payload); }
    catch { /* The bounded in-memory cache still works if storage is full. */ }
  } catch { /* Ignore malformed or non-JSON results rather than breaking Discover. */ }
}

export function readDiscoverPreferences(): DiscoverPreferences {
  return { ...stateFor(storage()).preferences };
}
export function writeDiscoverPreferences(value: DiscoverPreferences): void {
  const target = storage();
  const state = stateFor(target);
  state.preferences = preferences(value);
  try { target?.setItem(PREFERENCES_KEY, JSON.stringify(state.preferences)); }
  catch { /* Preferences remain available for the current app session. */ }
}

function nameOf(model: JsonObject): string {
  return String(model.id ?? model.modelId ?? model.name ?? '');
}
function numeric(value: unknown): number | null {
  const number = typeof value === 'number' ? value : typeof value === 'string' && /^\d+(?:\.\d+)?$/.test(value) ? Number(value) : NaN;
  return Number.isFinite(number) && number >= 0 ? number : null;
}
function quantizationBits(value: unknown): number | null {
  if (typeof value !== 'string') return null;
  const match = value.match(/(?:^|[^a-z0-9])(\d{1,2}(?:\.\d+)?)[\s_-]*bits?(?=$|[^a-z])/i)
    ?? value.match(/(?:^|[^a-z0-9])q(\d{1,2})(?=$|[_-]|[a-z])/i)
    ?? value.match(/(?:^|[^a-z0-9])(?:bf|fp|bfloat|float|u?int)(\d{1,2})(?=$|[^a-z0-9])/i);
  const bits = match ? Number(match[1]) : NaN;
  return Number.isFinite(bits) && bits > 0 && bits <= 64 ? bits : null;
}
function metric(model: JsonObject, sort: CatalogueSort): { value: number | null; rank: number } {
  if (sort === 'downloads') {
    const value = numeric(model.downloads);
    return { value, rank: value === null ? 2 : 0 };
  }
  if (sort === 'parameters') {
    const value = modelParameters(model).count;
    return { value, rank: value === null ? 2 : 0 };
  }
  // Metadata may be incomplete while results are loading; malformed optional data
  // should leave the value unknown rather than make the whole sort fail.
  const safe = {
    ...model,
    id: nameOf(model),
    tags: Array.isArray(model.tags) ? model.tags.filter((tag: unknown) => typeof tag === 'string') : [],
    cardData: record(model.cardData) ? model.cardData : {},
    studio_size: record(model.studio_size) ? model.studio_size : {},
  };
  const metadata = modelMetadata(safe);
  if (sort === 'bits') {
    const candidates = [metadata.quantization, model.quantization, ...safe.tags, nameOf(model)];
    const value = candidates.map(quantizationBits).find(bits => bits !== null) ?? null;
    return { value, rank: value === null ? 2 : 0 };
  }
  const value = metadata.downloadBytes;
  const complete = metadata.downloadComplete;
  return { value, rank: value === null ? 2 : complete ? 0 : 1 };
}
function compareNames(left: string, right: string): number {
  const a = left.toLowerCase();
  const b = right.toLowerCase();
  return a < b ? -1 : a > b ? 1 : left < right ? -1 : left > right ? 1 : 0;
}

// Display the same known precision that numeric sorting uses, including dtype-only tags.
export function catalogueBits(model: JsonObject): number | null {
  return metric(model, 'bits').value;
}

export function sortCatalogue(results: JsonObject[], sort: CatalogueSort, direction: CatalogueDirection): JsonObject[] {
  const sign = direction === 'asc' ? 1 : -1;
  return results.map((model, index) => ({ model, index, name: nameOf(model), ...metric(model, sort) }))
    .sort((left, right) => left.rank - right.rank
      || (left.value !== null && right.value !== null ? (left.value - right.value) * sign : 0)
      || compareNames(left.name, right.name)
      || left.index - right.index)
    .map(item => item.model);
}
