import assert from 'node:assert/strict';
import test from 'node:test';
import {
  readCatalogue, readDiscoverPreferences, sortCatalogue,
  writeCatalogue, writeDiscoverPreferences,
  publicationDate, publicationTimestamp, filterPublicationDates, catalogueCompatibility,
} from '../src/catalogue';

const CACHE_KEY = 'studio.discover.catalogue.v1';
const PREFERENCES_KEY = 'studio.discover.preferences';
function storage(initial: Record<string, string> = {}) {
  const values = new Map(Object.entries(initial));
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: { getItem: (key: string) => values.get(key) ?? null, setItem: (key: string, value: string) => { values.set(key, value); } },
  });
  return values;
}
const ids = (results: Record<string, any>[]) => results.map(model => model.id);

test('catalogue snapshots survive a new storage session and preserve timestamps for stale revalidation', () => {
  const values = storage();
  const input = [{ id: 'publisher/Model', tags: ['mlx'], studio_size: { weights_bytes: 42 } }];
  const before = Date.now();
  writeCatalogue('  Qwen  ', input);
  const entry = readCatalogue('qwen');
  assert.ok(entry && entry.updatedAt >= before && entry.updatedAt <= Date.now());
  input[0].studio_size.weights_bytes = 99;
  entry.results[0].studio_size.weights_bytes = 123;
  assert.equal(readCatalogue('QWEN')?.results[0].studio_size.weights_bytes, 42, 'callers cannot mutate a cached snapshot');

  const saved = JSON.parse(values.get(CACHE_KEY)!);
  saved.entries[0].updatedAt = Date.now() - 600_000;
  storage({ [CACHE_KEY]: JSON.stringify(saved) });
  assert.equal(readCatalogue('qwen')?.updatedAt, saved.entries[0].updatedAt, 'stale data remains available to render during refresh');
  assert.equal(readCatalogue('qwen')?.results[0].id, 'publisher/Model');
});

test('publication dates use createdAt only, preserve UTC boundaries, and reject unavailable or malformed dates', () => {
  assert.equal(publicationDate({ createdAt: '2026-10-25T23:30:00-02:00' })?.day, '2026-10-26');
  assert.equal(publicationDate({ createdAt: '2026-10-26T00:30:00+02:00' })?.day, '2026-10-25');
  assert.equal(publicationDate({ created_at: '2024-02-29T12:00:00Z' })?.label, '29 Feb 2024');
  for (const model of [{}, { lastModified: '2026-10-26T10:00:00Z' }, { createdAt: '2026-02-30T00:00:00Z' }, { createdAt: '2026-10-26T24:00:00Z' }, { createdAt: 'yesterday' }, { createdAt: '2026-10-26T10:00:00' }]) assert.equal(publicationTimestamp(model), null);
});

test('publication filters include both entire UTC boundary days and omit unknown dates only while active', () => {
  const models = [
    { id: 'old', createdAt: '2026-10-24T23:59:59Z' },
    { id: 'first', createdAt: '2026-10-25T00:00:00Z' },
    { id: 'last', createdAt: '2026-10-26T23:59:59.999Z' },
    { id: 'new', createdAt: '2026-10-27T00:00:00Z' },
    { id: 'unknown', lastModified: '2026-10-26T10:00:00Z' },
  ];
  assert.deepEqual(ids(filterPublicationDates(models, '2026-10-25', '2026-10-26')), ['first', 'last']);
  assert.deepEqual(ids(filterPublicationDates(models, '', '2026-10-25')), ['old', 'first']);
  assert.deepEqual(ids(filterPublicationDates(models, '2026-10-26')), ['last', 'new']);
  assert.deepEqual(ids(filterPublicationDates(models)), ids(models));
  assert.deepEqual(filterPublicationDates(models, '2026-10-27', '2026-10-25'), []);
});

test('published sorting is chronological with unknown dates last in either direction', () => {
  const models = [{ id: 'unknown', lastModified: '2027-01-01T00:00:00Z' }, { id: 'new', createdAt: '2026-10-26T00:00:00Z' }, { id: 'old', createdAt: '2025-01-01T00:00:00Z' }];
  assert.deepEqual(ids(sortCatalogue(models, 'published', 'desc')), ['new', 'old', 'unknown']);
  assert.deepEqual(ids(sortCatalogue(models, 'published', 'asc')), ['old', 'new', 'unknown']);
});

test('publication filters and sorting persist while invalid saved dates stay unset', () => {
  storage();
  const value = { query: 'Qwen', view: 'list', sort: 'published', direction: 'desc', dateFrom: '2026-01-01', dateTo: '2026-10-26' } as const;
  writeDiscoverPreferences(value);
  assert.deepEqual(readDiscoverPreferences(), value);
  storage({ [PREFERENCES_KEY]: JSON.stringify({ ...value, dateFrom: '2026-02-30', dateTo: 'bad' }) });
  const { dateFrom, dateTo, ...rest } = value;
  assert.deepEqual(readDiscoverPreferences(), rest);
});

test('only explicit backend compatibility for the displayed revision is accepted', () => {
  assert.equal(catalogueCompatibility({ tags: ['mlx'], library_name: 'mlx' }).status, 'unknown');
  assert.equal(catalogueCompatibility({ sha: 'a'.repeat(40), studio_compatibility: { status: 'compatible', revision: 'b'.repeat(40) } }).status, 'unknown');
  assert.equal(catalogueCompatibility({ sha: 'a'.repeat(40), studio_compatibility: { status: 'compatible', revision: 'a'.repeat(40) } }).status, 'compatible');
  assert.deepEqual(catalogueCompatibility({ studio_compatibility: { status: 'incompatible', reason: 'Missing tokenizer' } }), { status: 'incompatible', reason: 'Missing tokenizer' });
});

test('catalogue caps query history and evicts the least recently written search', () => {
  const values = storage();
  for (let index = 0; index < 8; index += 1) writeCatalogue(`query ${index}`, [{ id: `author/Model-${index}` }]);
  writeCatalogue('query 0', [{ id: 'author/Updated' }]);
  writeCatalogue('query 8', [{ id: 'author/New' }]);
  assert.equal(readCatalogue('query 1'), undefined);
  assert.equal(readCatalogue('query 0')?.results[0].id, 'author/Updated');
  assert.equal(JSON.parse(values.get(CACHE_KEY)!).entries.length, 8);
});

test('metadata enrichment can preserve the original search timestamp', () => {
  storage();
  const updatedAt = Date.now() - 120_000;
  writeCatalogue('enriched', [{ id: 'author/Model' }], updatedAt);
  writeCatalogue('enriched', [{ id: 'author/Model', studio_size: { weights_bytes: 42, weights_complete: true } }], updatedAt);
  assert.equal(readCatalogue('enriched')?.updatedAt, updatedAt);
  assert.equal(readCatalogue('enriched')?.results[0].studio_size.weights_bytes, 42);
});

test('catalogue bounds serialized storage and safely ignores a single oversized response', () => {
  const values = storage();
  for (let index = 0; index < 8; index += 1) writeCatalogue(`large ${index}`, [{ id: `author/${index}`, description: 'x'.repeat(150_000) }]);
  assert.ok(values.get(CACHE_KEY)!.length <= 500_000);
  assert.equal(readCatalogue('large 0'), undefined, 'size budget can evict entries before reaching eight queries');
  assert.ok(readCatalogue('large 7'));
  writeCatalogue('oversized', [{ id: 'author/Too-big', description: 'x'.repeat(500_000) }]);
  assert.equal(readCatalogue('oversized'), undefined);
  assert.ok(readCatalogue('large 7'), 'an oversized response does not destroy smaller cached searches');
});

test('catalogue rejects malformed persisted entries and invalid JSON without losing valid entries', () => {
  const updatedAt = Date.now();
  storage({ [CACHE_KEY]: JSON.stringify({ version: 1, entries: [
    { query: 'valid', updatedAt, results: [{ id: 'author/Good' }] },
    { query: 'future', updatedAt: updatedAt + 600_000, results: [{ id: 'author/Future' }] },
    { query: 'missing id', updatedAt, results: [{}] },
    { query: 'bad tags', updatedAt, results: [{ id: 'author/Bad', tags: 'mlx' }] },
    { query: 'bad time', updatedAt: 'yesterday', results: [{ id: 'author/Bad' }] },
  ] }) });
  assert.ok(readCatalogue('valid'));
  for (const query of ['future', 'missing id', 'bad tags', 'bad time']) assert.equal(readCatalogue(query), undefined);
  storage({ [CACHE_KEY]: '{broken' });
  assert.equal(readCatalogue('valid'), undefined);
  assert.doesNotThrow(() => writeCatalogue('circular', [{ id: 'author/BigInt', value: BigInt(1) }]));
  assert.equal(readCatalogue('circular'), undefined);
});

test('preferences persist all Discover controls and validate unexpected saved values', () => {
  const values = storage();
  assert.deepEqual(readDiscoverPreferences(), { query: '', view: 'grid', sort: 'downloads', direction: 'desc' });
  const preferences = { query: 'Qwen', view: 'list', sort: 'disk', direction: 'asc' } as const;
  writeDiscoverPreferences(preferences);
  storage(Object.fromEntries(values));
  assert.deepEqual(readDiscoverPreferences(), preferences);
  const copy = readDiscoverPreferences();
  copy.query = 'Changed';
  assert.equal(readDiscoverPreferences().query, 'Qwen');
  const escaped = { ...preferences, query: '\\'.repeat(2_048) };
  writeDiscoverPreferences(escaped);
  storage({ [PREFERENCES_KEY]: localStorage.getItem(PREFERENCES_KEY)! });
  assert.deepEqual(readDiscoverPreferences(), escaped, 'JSON escaping does not invalidate an otherwise bounded query');
  storage({ [PREFERENCES_KEY]: JSON.stringify({ query: 10, view: 'table', sort: 'likes', direction: 'up' }) });
  assert.deepEqual(readDiscoverPreferences(), { query: '', view: 'grid', sort: 'downloads', direction: 'desc' });
});

test('legacy weight sorting migrates to parameters without changing other Discover preferences', () => {
  storage({ [PREFERENCES_KEY]: JSON.stringify({ query: 'Qwen 0.8B', view: 'list', sort: 'weights', direction: 'asc' }) });
  const migrated = readDiscoverPreferences();
  assert.deepEqual(migrated, { query: 'Qwen 0.8B', view: 'list', sort: 'parameters', direction: 'asc' });
  writeDiscoverPreferences(migrated);
  assert.equal(JSON.parse(localStorage.getItem(PREFERENCES_KEY)!).sort, 'parameters');
  storage({ [PREFERENCES_KEY]: JSON.stringify({ query: 'Gemma', view: 'list', sort: 'disk', direction: 'desc' }) });
  assert.deepEqual(readDiscoverPreferences(), { query: 'Gemma', view: 'list', sort: 'disk', direction: 'desc' }, 'download-size sorting retains its existing key');
});

test('blocked or full local storage keeps session cache and preferences usable', () => {
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: { getItem() { throw new Error('Blocked'); }, setItem() { throw new Error('Quota exceeded'); } },
  });
  assert.doesNotThrow(() => writeCatalogue('offline', [{ id: 'author/Offline' }]));
  assert.equal(readCatalogue('offline')?.results[0].id, 'author/Offline');
  writeDiscoverPreferences({ query: 'offline', view: 'list', sort: 'bits', direction: 'asc' });
  assert.equal(readDiscoverPreferences().view, 'list');
  Object.defineProperty(globalThis, 'localStorage', { configurable: true, get() { throw new Error('No storage access'); } });
  assert.doesNotThrow(() => writeCatalogue('no storage', [{ id: 'author/In-memory' }]));
  assert.equal(readCatalogue('no storage')?.results[0].id, 'author/In-memory');
  storage();
  assert.equal(readCatalogue('offline'), undefined, 'independent storage sessions do not share module cache state');
});

test('downloads sort numerically in both directions, with missing values always last', () => {
  const results = [
    { id: 'author/Unknown' }, { id: 'author/Ten', downloads: '10' },
    { id: 'author/Two', downloads: 2 }, { id: 'author/Zero', downloads: 0 },
    { id: 'author/Invalid', downloads: -1 },
  ];
  assert.deepEqual(ids(sortCatalogue(results, 'downloads', 'asc')), ['author/Zero', 'author/Two', 'author/Ten', 'author/Invalid', 'author/Unknown']);
  assert.deepEqual(ids(sortCatalogue(results, 'downloads', 'desc')), ['author/Ten', 'author/Two', 'author/Zero', 'author/Invalid', 'author/Unknown']);
  assert.equal(results[0].id, 'author/Unknown', 'sorting does not mutate the input array');
});

test('parameter sorting orders nominal 0.8B, 2B, 9B and 37B sizes with unknown models last in both directions', () => {
  const results = [
    { id: 'author/Unknown-4bit', safetensors: { total: 100_000_000_000 }, studio_size: { weights_bytes: 100_000_000_000, weights_complete: true } },
    { id: 'author/Model-9B-4bit', safetensors: { total: 1 }, studio_size: { weights_bytes: 1, weights_complete: true } },
    { id: 'author/Model-0.8B-8bit', safetensors: { total: 1_000_000_000_000 }, studio_size: { weights_bytes: 1_000_000_000_000, weights_complete: false } },
    { id: 'author/Model-37B-2bit', siblings: [{ rfilename: 'model.safetensors', size: 2 }] },
    { id: 'author/Model-2B-fp16' },
  ];
  assert.deepEqual(ids(sortCatalogue(results, 'parameters', 'asc')), ['author/Model-0.8B-8bit', 'author/Model-2B-fp16', 'author/Model-9B-4bit', 'author/Model-37B-2bit', 'author/Unknown-4bit']);
  assert.deepEqual(ids(sortCatalogue(results, 'parameters', 'desc')), ['author/Model-37B-2bit', 'author/Model-9B-4bit', 'author/Model-2B-fp16', 'author/Model-0.8B-8bit', 'author/Unknown-4bit']);
});

test('file bytes and stored Safetensors element totals cannot supply nominal parameter counts', () => {
  const results = [
    { id: 'author/A-unknown', safetensors: { total: 999_000_000_000 } },
    { id: 'author/B-unknown', studio_size: { weights_bytes: 2_000_000_000, weights_complete: true, download_bytes: 3_000_000_000, download_complete: true } },
    { id: 'author/Model-0.8B-4bit', safetensors: { total: 1 }, studio_size: { weights_bytes: 1, weights_complete: false } },
    { id: 'author/C-unknown', siblings: [{ rfilename: 'model.safetensors', size: 37_000_000_000 }] },
  ];
  for (const direction of ['asc', 'desc'] as const) {
    assert.deepEqual(ids(sortCatalogue(results, 'parameters', direction)), ['author/Model-0.8B-4bit', 'author/A-unknown', 'author/B-unknown', 'author/C-unknown']);
  }
});

test('disk sorting uses total download bytes rather than weights or parameter counts', () => {
  const results = [
    { id: 'author/A', studio_size: { weights_bytes: 1000, download_bytes: 20, download_complete: true } },
    { id: 'author/B', studio_size: { weights_bytes: 1, download_bytes: 100, download_complete: true } },
    { id: 'author/Partial', studio_size: { download_bytes: 1, download_complete: false } },
    { id: 'author/Unknown', siblings: [{ rfilename: 'model.safetensors', size: 2 }] },
  ];
  assert.deepEqual(ids(sortCatalogue(results, 'disk', 'asc')), ['author/A', 'author/B', 'author/Partial', 'author/Unknown']);
  assert.deepEqual(ids(sortCatalogue(results, 'disk', 'desc')), ['author/B', 'author/A', 'author/Partial', 'author/Unknown']);
});

test('bit sorting recognizes named, tagged and card quantizations without treating parameter counts as bits', () => {
  const results = [
    { id: 'author/Model-0.8B' },
    { id: 'author/Model-fp16' },
    { id: 'author/Model-q4' },
    { id: 'author/Model', tags: ['Q8_0'] },
    { id: 'author/Model-3-bit' },
    { id: 'author/Model-4bit' },
    { id: 'author/Bfloat', cardData: { quantization: 'bf16' } },
    { id: 'author/Fractional', cardData: { quantization: '4.5 bit' } },
  ];
  assert.deepEqual(ids(sortCatalogue(results, 'bits', 'asc')), ['author/Model-3-bit', 'author/Model-4bit', 'author/Model-q4', 'author/Fractional', 'author/Model', 'author/Bfloat', 'author/Model-fp16', 'author/Model-0.8B']);
  assert.deepEqual(ids(sortCatalogue(results, 'bits', 'desc')), ['author/Bfloat', 'author/Model-fp16', 'author/Model', 'author/Fractional', 'author/Model-4bit', 'author/Model-q4', 'author/Model-3-bit', 'author/Model-0.8B']);
});

test('equal metrics use deterministic ascending names and preserve duplicates in input order', () => {
  const first = { id: 'author/A', downloads: 1, marker: 'first' };
  const second = { id: 'author/A', downloads: 1, marker: 'second' };
  const results = [{ id: 'author/z', downloads: 1 }, second, { id: 'author/b', downloads: 1 }, first];
  for (const direction of ['asc', 'desc'] as const) {
    const sorted = sortCatalogue(results, 'downloads', direction);
    assert.deepEqual(ids(sorted), ['author/A', 'author/A', 'author/b', 'author/z']);
    assert.equal(sorted[0], second);
    assert.equal(sorted[1], first);
  }
});
