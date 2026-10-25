import React from 'react';
import assert from 'node:assert/strict';
import test from 'node:test';
import TestRenderer, { act } from 'react-test-renderer';
import ModelLogo from '../src/ModelLogo';

const PNG = 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aBz8AAAAASUVORK5CYII=';
const flush = () => new Promise(resolve => setImmediate(resolve));
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(complete => { resolve = complete; });
  return { promise, resolve };
}
function bridge(handle: (repoId: string) => any) {
  const calls: string[] = [];
  Object.assign(globalThis, {
    window: { __TAURI_INTERNALS__: { async invoke(command: string, args: any) {
      assert.equal(command, 'studio_request');
      assert.equal(args.method, 'model.logo');
      calls.push(args.params.repo_id);
      return handle(args.params.repo_id);
    } } },
  });
  return calls;
}

test('model logos share pending and cached publisher requests across different repositories', async () => {
  const pending = deferred<{ data_url: string }>();
  const calls = bridge(() => pending.promise);
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<><ModelLogo repoId="logo-shared/one" /><ModelLogo repoId="logo-shared/two" /></>); await flush(); });
  try {
    assert.deepEqual(calls, ['logo-shared/one']);
    assert.equal(renderer.root.findAllByType('img').length, 0);
    await act(async () => { pending.resolve({ data_url: PNG }); await flush(); });
    assert.equal(renderer.root.findAllByType('img').length, 2);
    assert.ok(renderer.root.findAllByType('img').every(image => image.props.src === PNG));
    await act(async () => { renderer.update(<ModelLogo repoId="logo-shared/three" size={48} className="library-model-icon" />); await flush(); });
    assert.equal(calls.length, 1);
    assert.equal(renderer.root.findByType('img').props.src, PNG);
    assert.deepEqual(renderer.root.findByType('span').props.style, { width: 48, height: 48 });
  } finally { pending.resolve({ data_url: PNG }); await act(async () => renderer.unmount()); }
});

test('a recognized model family falls back to its repository publisher and caches missing avatars', async () => {
  const calls = bridge(repoId => repoId === 'google/avatar' ? { data_url: null } : { data_url: PNG });
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ModelLogo repoId="logo-gemma-publisher/gemma-3-4bit" />); await flush(); });
  try {
    assert.deepEqual(calls, ['google/avatar', 'logo-gemma-publisher/gemma-3-4bit']);
    assert.equal(renderer.root.findByType('img').props.src, PNG);
    assert.match(renderer.root.findByType('span').props.title, /logo-gemma-publisher publisher avatar/);
    await act(async () => { renderer.update(<ModelLogo repoId="logo-gemma-publisher/gemma-4-4bit" />); await flush(); });
    assert.equal(calls.length, 2, 'missing family avatars and successful publisher images are reused');
  } finally { await act(async () => renderer.unmount()); }
});

test('image decode failures fall back once to the publisher and then to the generic chip', async () => {
  const calls = bridge(() => ({ data_url: PNG }));
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ModelLogo repoId="logo-decode/deepseek-r1-4bit" />); await flush(); });
  try {
    assert.deepEqual(calls, ['deepseek-ai/avatar']);
    assert.match(renderer.root.findByType('span').props.title, /DeepSeek model family avatar/);
    await act(async () => { renderer.root.findByType('img').props.onError(); await flush(); });
    assert.deepEqual(calls, ['deepseek-ai/avatar', 'logo-decode/deepseek-r1-4bit']);
    assert.match(renderer.root.findByType('span').props.title, /logo-decode publisher avatar/);
    await act(async () => { renderer.root.findByType('img').props.onError(); await flush(); });
    assert.equal(renderer.root.findAllByType('img').length, 0);
    assert.equal(renderer.root.findAllByType('svg').length, 1);
    await act(async () => { renderer.update(<ModelLogo repoId="logo-decode/deepseek-v3-4bit" />); await flush(); });
    assert.equal(calls.length, 2, 'broken images are negatively cached without a request loop');
  } finally { await act(async () => renderer.unmount()); }
});

test('logo fetch concurrency is bounded and offline or unknown local models keep the generic chip', async () => {
  const pending = Array.from({ length: 10 }, () => deferred<{ data_url: null }>());
  let active = 0; let peak = 0;
  const calls = bridge(repoId => {
    const index = Number(repoId.split('/')[0].split('-').at(-1));
    active++; peak = Math.max(peak, active);
    return pending[index].promise.finally(() => { active--; });
  });
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<>{pending.map((_, index) => <ModelLogo key={index} repoId={`logo-queue-${index}/model`} />)}<ModelLogo name="Unknown local model" /></>); await flush(); });
  try {
    assert.equal(calls.length, 4);
    assert.equal(peak, 4);
    await act(async () => { pending.slice(0, 4).forEach(item => item.resolve({ data_url: null })); await flush(); });
    assert.equal(calls.length, 8);
    await act(async () => { pending.forEach(item => item.resolve({ data_url: null })); await flush(); });
    assert.equal(calls.length, 10);
    assert.equal(peak, 4);
    assert.equal(renderer.root.findAllByType('img').length, 0);
    assert.equal(renderer.root.findAllByType('svg').length, 11);
  } finally { pending.forEach(item => item.resolve({ data_url: null })); await act(async () => renderer.unmount()); }
});

test('network errors and unsupported image payloads do not replace the generic chip', async () => {
  const calls = bridge(repoId => {
    if (repoId.startsWith('logo-offline/')) throw new Error('offline');
    return { data_url: 'data:image/svg+xml;base64,PHN2Zz48L3N2Zz4=' };
  });
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<><ModelLogo repoId="logo-offline/model" /><ModelLogo repoId="logo-unsupported/model" /></>); await flush(); });
  try {
    assert.equal(renderer.root.findAllByType('img').length, 0);
    assert.equal(renderer.root.findAllByType('svg').length, 2);
    await act(async () => { renderer.update(<><ModelLogo repoId="logo-offline/another" /><ModelLogo repoId="logo-unsupported/another" /></>); await flush(); });
    assert.equal(calls.length, 2);
  } finally { await act(async () => renderer.unmount()); }
});
