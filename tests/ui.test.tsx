import React from 'react';
import assert from 'node:assert/strict';
import test from 'node:test';
import TestRenderer, { act } from 'react-test-renderer';
import App from '../src/App';
import LibraryView from '../src/views/LibraryView';
import ServerView from '../src/views/ServerView';
import { Markdown } from '../src/components';
import { EMPTY_SNAPSHOT } from '../src/types';

const model = { id: 'm1', name: 'Test model', path: '/models/test', source: 'import', weights_bytes: 1024, compatible: true };
const profile = { id: 'p1', model_id: 'm1', alias: 'test', selected: true, options: { max_context: 8192 }, env: {} };
const snapshot = () => ({ ...structuredClone(EMPTY_SNAPSHOT), models: [model], profiles: [profile], servers: [{ profile_id: 'p1', model_id: 'm1', alias: 'test', state: 'running', active_requests: 0 }], telemetry: { system: {}, workers: [], managed_totals: {} } });
const flush = () => new Promise(resolve => setImmediate(resolve));

function harness() {
  let data = snapshot();
  let eventHandler: ((event: any) => void) | undefined;
  const callbacks = new Map<number, (event: any) => void>();
  const calls: { method: string; params: any }[] = [];
  let finishStart: ((value: any) => void) | undefined;
  const storage = new Map<string, string>();
  Object.assign(globalThis, {
    localStorage: { getItem: (key: string) => storage.get(key), setItem: (key: string, value: string) => storage.set(key, value) },
    document: { hidden: false, documentElement: { dataset: {} }, querySelector: () => null, activeElement: null },
    window: {
      setInterval, clearInterval, setTimeout, clearTimeout,
      addEventListener() {}, removeEventListener() {}, innerHeight: 900,
      __TAURI_EVENT_PLUGIN_INTERNALS__: { unregisterListener() {} },
      __TAURI_INTERNALS__: {
        transformCallback(callback: (event: any) => void) { const id = callbacks.size + 1; callbacks.set(id, callback); return id; },
        async invoke(command: string, args: any) {
          if (command === 'plugin:event|listen') { eventHandler = callbacks.get(args.handler); return 1; }
          if (command !== 'studio_request') return;
          const { method, params } = args;
          calls.push({ method, params });
          if (method === 'snapshot' || method === 'status') return structuredClone(data);
          if (method === 'conversations.save') { data.conversations = [...data.conversations.filter(item => item.id !== params.conversation.id), structuredClone(params.conversation)]; return params.conversation; }
          if (method === 'folders.save') { data.folders = [...data.folders.filter(item => item.id !== params.folder.id), structuredClone(params.folder)]; return params.folder; }
          if (method === 'folders.remove') { data.folders = data.folders.filter(item => item.id !== params.id); return { removed: params.id }; }
          if (method === 'chat.start') return new Promise(resolve => { finishStart = resolve; });
          if (method === 'memory.estimate') return { total_estimated_bytes: 0, budget_bytes: 100, complete: false, warnings: [], requires_override: false };
          if (method === 'server.logs') return { text: 'Worker starting\nListening on localhost', path: '/tmp/server.log', truncated: false };
          if (method === 'options.schema') return { fields: [{ key: 'thinking', type: 'boolean', label: 'Thinking', default: false, description: 'Think before responding.' }] };
          return {};
        },
      },
    },
  });
  return {
    calls,
    emit(type: string, value: any) { eventHandler?.({ payload: { type, data: value } }); },
    get data() { return data; },
    resolveStart(value: any) { assert.ok(finishStart, 'chat.start reached the bridge'); finishStart(value); },
  };
}

test('chat drafts survive navigation and a completed stream cannot be resurrected by its delayed start reply', async () => {
  const bridge = harness();
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: 'Keep this draft' } }); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'My models' }).props.onClick(); await flush(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Chat' }).props.onClick(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Message' }).props.value, 'Keep this draft');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Send message' }).props.onClick(); await flush(); });
    const start = bridge.calls.find(call => call.method === 'chat.start');
    assert.ok(start);
    const conversationId = start.params.conversation_id;
    const completed = { role: 'assistant', content: 'A streamed answer.', usage: { completion_tokens: 5 } };
    await act(async () => {
      bridge.emit('chat.delta', { request_id: 'r1', conversation_id: conversationId, delta: 'A streamed' });
      await flush();
    });
    assert.ok(renderer.root.findAllByType(Markdown).some(node => node.props.children === 'A streamed'));
    bridge.data.conversations[0].messages.push(completed);
    await act(async () => {
      bridge.emit('chat.done', { request_id: 'r1', conversation_id: conversationId, message: completed });
      await flush();
      bridge.resolveStart({ request_id: 'r1', conversation_id: conversationId });
      await flush();
    });
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Stop generating' }).length, 0);
    assert.ok(renderer.root.findAllByType(Markdown).some(node => node.props.children === completed.content));
    assert.equal(bridge.calls.some(call => call.method === 'search'), false, 'no Hub request before opening Discover');
    const before = bridge.calls.filter(call => call.method === 'snapshot').length;
    await act(async () => { for (let i = 0; i < 20; i++) bridge.emit('telemetry', { system: { cpu_percent: i } }); });
    assert.equal(bridge.calls.filter(call => call.method === 'snapshot').length, before, 'telemetry patches state without expensive snapshot reads');
  } finally { await act(async () => renderer.unmount()); }
});

test('profile form omits cleared overrides and synchronizes boolean controls with the saved options', async () => {
  harness();
  const calls: any[] = [];
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<LibraryView snapshot={snapshot()} onAction={async (method, params) => { calls.push({ method, params }); return {}; }} onSearch={() => {}} />); });
  try {
    await act(async () => { renderer.root.findAllByType('button').find(node => String(node.props.className).startsWith('library-model '))!.props.onClick(); });
    await act(async () => { renderer.root.findAllByType('button').find(node => node.props.children === 'Server profiles')!.props.onClick(); await flush(); });
    await act(async () => { renderer.root.findAllByType('input').find(node => node.props.value === 8192)!.props.onChange({ target: { value: '' } }); });
    await act(async () => { renderer.root.findByProps({ className: 'advanced-toggle' }).props.onClick(); });
    await act(async () => { renderer.root.findAllByType('select').find(node => node.props.children?.some?.((child: any) => child?.props?.value === 'true'))!.props.onChange({ target: { value: 'true' } }); });
    await act(async () => { await renderer.root.findByProps({ className: 'inspector-body profile-editor' }).props.onSubmit({ preventDefault() {} }); });
    const saved = calls.find(call => call.method === 'profiles.save').params.profile;
    assert.equal('max_context' in saved.options, false);
    assert.equal(saved.options.thinking, true);
    assert.equal('slots' in saved.options, false);
  } finally { await act(async () => renderer.unmount()); }
});

test('server view renders backend text logs and exposes keyboard resizing', async () => {
  harness();
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={snapshot()} onAction={async () => ({})} onLibrary={() => {}} />); await flush(); });
  try {
    const lines = renderer.root.findByProps({ 'aria-label': 'Server logs', role: 'log' });
    const text = (node: any): string => typeof node === 'string' ? node : (node.children ?? []).map(text).join(' ');
    assert.match(text(lines), /Listening on localhost/);
    const handle = renderer.root.findByProps({ 'aria-label': 'Resize server logs' });
    const oldHeight = handle.props['aria-valuenow'];
    await act(async () => { handle.props.onKeyDown({ key: 'ArrowUp', preventDefault() {} }); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Resize server logs' }).props['aria-valuenow'], oldHeight + 20);
  } finally { await act(async () => renderer.unmount()); }
});

test('folder creation and rename keep the persisted folder identity', async () => {
  const bridge = harness();
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'New folder' }).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Folder name' }).props.onChange({ target: { value: 'Writing' } }); });
    await act(async () => { renderer.root.findByProps({ className: 'folder-form' }).props.onSubmit({ preventDefault() {} }); await flush(); });
    const id = bridge.data.folders[0].id;
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Rename folder Writing' }).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Folder name' }).props.onChange({ target: { value: 'Projects' } }); });
    await act(async () => { renderer.root.findByProps({ className: 'folder-form' }).props.onSubmit({ preventDefault() {} }); await flush(); });
    assert.equal(bridge.data.folders.length, 1);
    assert.equal(bridge.data.folders[0].id, id);
    assert.equal(bridge.data.folders[0].name, 'Projects');
    assert.ok(renderer.root.findByProps({ 'aria-label': 'New conversation in Projects' }));
  } finally { await act(async () => renderer.unmount()); }
});

test('a model loading in a pending start request can still be stopped', async () => {
  harness();
  const data = snapshot(); data.servers[0].state = 'stopped';
  const calls: string[] = [];
  let resolveStart: (value: any) => void;
  const onAction = async (method: string) => { calls.push(method); if (method === 'server.start') return new Promise(resolve => { resolveStart = resolve; }); return {}; };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={data} onAction={onAction} onLibrary={() => {}} />); await flush(); });
  try {
    await act(async () => { renderer.root.findByProps({ className: 'start-server' }).props.onClick(); await flush(); });
    data.servers[0].state = 'starting';
    await act(async () => { renderer.update(<ServerView snapshot={structuredClone(data)} onAction={onAction} onLibrary={() => {}} />); await flush(); });
    const stop = renderer.root.findByProps({ className: 'stop-server' });
    assert.equal(stop.props.disabled, false);
    await act(async () => { stop.props.onClick(); await flush(); });
    assert.deepEqual(calls, ['server.start', 'server.stop']);
    await act(async () => { resolveStart!({ state: 'stopped' }); await flush(); });
  } finally { await act(async () => renderer.unmount()); }
});
