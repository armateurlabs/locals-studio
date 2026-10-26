import React from 'react';
import assert from 'node:assert/strict';
import test from 'node:test';
import TestRenderer, { act } from 'react-test-renderer';
import SearchView from '../src/views/SearchView';
import { writeCatalogue, writeDiscoverPreferences } from '../src/catalogue';
import { EMPTY_SNAPSHOT } from '../src/types';

const flush = () => new Promise(resolve => setImmediate(resolve));
const text = (node: any): string => typeof node === 'string' ? node : (node.children ?? []).map(text).join(' ').replace(/\s+/g, ' ');
function deferred<T>() {
  let resolve!: (value: T) => void, reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}
function model(id: string, patch: Record<string, any> = {}) {
  const sha = 'd'.repeat(40);
  return { id, sha, createdAt: '2026-10-25T00:00:00Z', studio_compatibility: { status: 'compatible', revision: sha }, studio_size: { revision: sha, download_bytes: 100, download_complete: true }, ...patch };
}
function harness(query: string, models: Record<string, any>[], info: (params: any) => any = params => models.find(value => value.id === params.repo_id)) {
  const storage = new Map<string, string>();
  const timers = new Map<number, () => void>();
  const calls: { method: string; params: any }[] = [];
  let timerId = 0;
  Object.assign(globalThis, {
    localStorage: { getItem: (key: string) => storage.get(key) ?? null, setItem: (key: string, value: string) => storage.set(key, value) },
    document: { activeElement: null },
    window: {
      setTimeout(callback: () => void) { const id = ++timerId; timers.set(id, callback); return id; },
      clearTimeout(id: number) { timers.delete(id); },
      __TAURI_INTERNALS__: { async invoke(command: string, args: any) {
        if (command !== 'studio_request') return;
        const { method, params } = args;
        calls.push({ method, params });
        if (method === 'model.info') return info(params);
        if (method === 'model.logo') return { data_url: null };
        if (method === 'search') return models;
        return {};
      } },
    },
  });
  writeDiscoverPreferences({ query, view: 'list', sort: 'downloads', direction: 'desc' });
  writeCatalogue(query, models);
  return { storage, calls, async tick() { const pending = [...timers.values()]; timers.clear(); for (const callback of pending) callback(); await flush(); } };
}
const cards = (renderer: TestRenderer.ReactTestRenderer) => renderer.root.findAllByProps({ className: 'discover-list-row' });
const props = { snapshot: EMPTY_SNAPSHOT, onAction: async () => ({}) };

test('Discover displays publication dates and persists inclusive UTC date filters and date sorting', async () => {
  const models = [model('dates-ui/Old-2B', { createdAt: '2026-10-24T23:59:59Z' }), model('dates-ui/Boundary-2B', { createdAt: '2026-10-26T00:30:00+02:00' }), model('dates-ui/New-2B', { createdAt: '2026-10-25T23:30:00-02:00' }), model('dates-ui/Unknown-2B', { createdAt: undefined, lastModified: '2026-10-25T00:00:00Z' })];
  const bridge = harness('dates-ui-query', models);
  let renderer!: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<SearchView {...props} />); await flush(); });
  try {
    assert.match(text(renderer.root.findByProps({ 'aria-label': 'Publication date for dates-ui/Boundary-2B' })), /25 Oct 2026/);
    assert.match(text(renderer.root.findByProps({ 'aria-label': 'Publication date for dates-ui/New-2B' })), /26 Oct 2026/);
    assert.match(text(renderer.root.findByProps({ 'aria-label': 'Publication date for dates-ui/Unknown-2B' })), /Unknown/);
    await act(async () => {
      renderer.root.findByProps({ 'aria-label': 'Published from' }).props.onChange({ target: { value: '2026-10-25' } });
      renderer.root.findByProps({ 'aria-label': 'Published through' }).props.onChange({ target: { value: '2026-10-25' } });
      renderer.root.findByProps({ 'aria-label': 'Sort by Published' }).props.onClick();
      await flush();
    });
    assert.deepEqual(cards(renderer).map(row => row.props['aria-label']), ['dates-ui/Boundary-2B']);
    const saved = JSON.parse(bridge.storage.get('studio.discover.preferences')!);
    assert.equal(saved.dateFrom, '2026-10-25'); assert.equal(saved.dateTo, '2026-10-25'); assert.equal(saved.sort, 'published');
    await act(async () => { renderer.unmount(); renderer = TestRenderer.create(<SearchView {...props} />); await flush(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Published from' }).props.value, '2026-10-25');
    assert.deepEqual(cards(renderer).map(row => row.props['aria-label']), ['dates-ui/Boundary-2B']);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Published through' }).props.onChange({ target: { value: '2026-10-24' } }); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Published through' }).props['aria-invalid'], true);
    assert.match(text(renderer.root.findByProps({ role: 'alert' })), /end date on or after/);
    assert.equal(cards(renderer).length, 0);
    await act(async () => { renderer.root.findAllByType('button').find(button => text(button) === 'Clear dates')!.props.onClick(); await flush(); });
    assert.equal(cards(renderer).length, 4);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Grid view' }).props.onClick(); });
    assert.match(text(renderer.root.findByProps({ 'aria-label': 'Publication date for dates-ui/Boundary-2B' })), /25 Oct 2026/);
    assert.match(text(renderer.root.findByProps({ 'aria-label': 'Publication date for dates-ui/Unknown-2B' })), /Unknown/);
    assert.equal(bridge.calls.filter(call => call.method === 'search').length, 0, 'date filtering remains within the returned catalogue');
  } finally { await act(async () => renderer.unmount()); }
});

test('Discover verifies unknown candidates with bounded requests, excludes incompatible results, and retries unknown failures', async () => {
  const models = Array.from({ length: 5 }, (_, index) => model(`compat-ui/Model-${index}-2B`, { studio_compatibility: { status: 'unknown' } }));
  const pending = new Map<string, ReturnType<typeof deferred<any>>>();
  const bridge = harness('compat-ui-query', models, params => { const request = deferred<any>(); pending.set(params.repo_id, request); return request.promise; });
  let renderer!: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<SearchView {...props} />); await flush(); });
  try {
    assert.equal(cards(renderer).length, 0);
    assert.equal(pending.size, 3);
    assert.match(text(renderer.root), /Checking 5 repositories/);
    await act(async () => { pending.get(models[0].id)!.resolve(model(models[0].id)); await flush(); });
    assert.equal(cards(renderer).length, 1);
    assert.equal(pending.size, 4, 'settling one check does not abandon the remaining queue');
    await act(async () => { pending.get(models[1].id)!.resolve(model(models[1].id, { studio_compatibility: { status: 'incompatible', reason: 'Missing tokenizer' } })); pending.get(models[2].id)!.reject(new Error('Hub unavailable')); await flush(); });
    assert.equal(pending.size, 5);
    await act(async () => { for (const value of models.slice(3)) pending.get(value.id)!.resolve(model(value.id)); await flush(); });
    assert.equal(cards(renderer).length, 3);
    assert.match(text(renderer.root), /1 incompatible repository excluded/);
    assert.match(text(renderer.root), /1 could not be verified/);
    assert.equal(cards(renderer).some(row => row.props['aria-label'] === models[1].id), false);
    await act(async () => { renderer.root.findAllByType('button').find(button => text(button) === 'Retry checks')!.props.onClick(); await flush(); });
    assert.equal(bridge.calls.filter(call => call.method === 'model.info' && call.params.repo_id === models[2].id).length, 2);
    await act(async () => { pending.get(models[2].id)!.resolve(model(models[2].id)); await flush(); });
    assert.equal(cards(renderer).length, 4);
  } finally { await act(async () => { renderer.unmount(); for (const [id, value] of pending) value.resolve(model(id)); await flush(); }); }
});

test('revision changes disable downloads until that revision is verified and stale replies cannot re-enable an incompatible download', async () => {
  const initial = model('revision-ui/Model-2B');
  const slow = deferred<any>();
  const nextSha = 'e'.repeat(40);
  const bridge = harness('revision-ui-query', [initial], params => {
    if (params.revision === initial.sha) return initial;
    if (params.revision === 'unsupported') return model(initial.id, { sha: 'f'.repeat(40), studio_compatibility: { status: 'incompatible', revision: 'f'.repeat(40), reason: 'Unsupported architecture' } });
    if (params.revision === 'offline') throw new Error('Unable to check this revision');
    if (params.revision === 'slow') return slow.promise;
    return model(initial.id, { sha: nextSha, studio_compatibility: { status: 'compatible', revision: nextSha }, studio_size: { revision: nextSha, download_bytes: 200, download_complete: true } });
  });
  const actions: any[] = [];
  let renderer!: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<SearchView snapshot={EMPTY_SNAPSHOT} onAction={async (method, params) => { actions.push({ method, params }); return {}; }} />); await flush(); });
  const revisionInput = () => renderer.root.findAllByType('input').find(input => input.props.placeholder === 'main')!;
  const downloadButton = () => renderer.root.findAllByType('button').find(button => button.props.className === 'primary' && /Download model/.test(text(button)))!;
  const selectRevision = async (revision: string) => { await act(async () => { revisionInput().props.onChange({ target: { value: revision } }); }); assert.equal(downloadButton().props.disabled, true); await act(async () => bridge.tick()); };
  try {
    await act(async () => { renderer.root.findByProps({ className: 'catalogue-get-model' }).props.onClick(); await flush(); });
    assert.equal(downloadButton().props.disabled, false);
    await selectRevision('offline');
    assert.match(text(renderer.root), /Unable to check this revision/);
    await act(async () => { revisionInput().props.onChange({ target: { value: initial.sha } }); await flush(); });
    assert.equal(downloadButton().props.disabled, false, 'restoring a validated compatible revision clears the failed revision’s error');
    assert.doesNotMatch(text(renderer.root), /Unable to check this revision/);
    await selectRevision('unsupported');
    assert.equal(downloadButton().props.disabled, true); assert.match(text(renderer.root), /Unsupported architecture/);
    await act(async () => { downloadButton().props.onClick(); await flush(); });
    assert.equal(actions.length, 0, 'an unverified or incompatible revision cannot be queued');
    await selectRevision('offline');
    assert.equal(downloadButton().props.disabled, true); assert.match(text(renderer.root), /Unable to check this revision/);
    await act(async () => { revisionInput().props.onChange({ target: { value: 'unsupported' } }); await flush(); });
    assert.equal(downloadButton().props.disabled, true);
    assert.doesNotMatch(text(renderer.root), /Unable to check this revision/, 'restoring the previously validated revision clears a different revision’s network error');
    assert.match(text(renderer.root), /Unsupported architecture/, 'the restored revision retains its verified incompatibility');
    await selectRevision('slow');
    await selectRevision('verified-branch');
    assert.equal(downloadButton().props.disabled, false);
    await act(async () => { slow.resolve(model(initial.id, { sha: 'f'.repeat(40), studio_compatibility: { status: 'incompatible', revision: 'f'.repeat(40), reason: 'Stale incompatible result' } })); await flush(); });
    assert.equal(downloadButton().props.disabled, false); assert.doesNotMatch(text(renderer.root), /Stale incompatible result/);
    await act(async () => { downloadButton().props.onClick(); await flush(); });
    assert.deepEqual(actions, [{ method: 'download.add', params: { repo_id: initial.id, revision: nextSha } }], 'download uses the verified immutable commit');
  } finally { await act(async () => { renderer.unmount(); slow.resolve(initial); await flush(); }); }
});
