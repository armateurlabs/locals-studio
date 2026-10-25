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
const snapshot = () => ({ ...structuredClone(EMPTY_SNAPSHOT), api: { running: true, enabled: true }, models: [model], profiles: [profile], servers: [{ profile_id: 'p1', model_id: 'm1', alias: 'test', state: 'running', active_requests: 0 }], telemetry: { system: {}, workers: [], managed_totals: {} } });
const flush = () => new Promise(resolve => setImmediate(resolve));

function harness(overrides: Record<string, (params: any) => any> = {}) {
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
          if (overrides[method]) return overrides[method](params);
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

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(complete => { resolve = complete; });
  return { promise, resolve: (value: T) => resolve(value) };
}

function controlledIntervals() {
  let created = 0;
  let cleared = 0;
  const timers = new Map<number, () => void>();
  window.setInterval = ((callback: () => void) => { const id = ++created; timers.set(id, callback); return id; }) as typeof window.setInterval;
  window.clearInterval = ((id: number) => { cleared += 1; timers.delete(id); }) as typeof window.clearInterval;
  return {
    get created() { return created; },
    get cleared() { return cleared; },
    tick() { for (const callback of [...timers.values()]) callback(); },
  };
}

function renderedText(node: any): string {
  return typeof node === 'string' ? node : (node.children ?? []).map(renderedText).join(' ');
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

test('telemetry rerenders and equivalent snapshots do not restart server reads or remove loaded content', async () => {
  const estimate = { total_estimated_bytes: 3 * 1024 ** 3, budget_bytes: 8 * 1024 ** 3, complete: false };
  const log = { text: 'Stable worker output', truncated: false };
  const pendingEstimate = deferred<any>();
  const pendingLogs = deferred<any>();
  let deferReads = false;
  const bridge = harness({
    'memory.estimate': () => deferReads ? pendingEstimate.promise : estimate,
    'server.logs': () => deferReads ? pendingLogs.promise : log,
  });
  const timers = controlledIntervals();
  const initial = structuredClone(snapshot());
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={initial} onAction={async () => ({})} onLibrary={() => {}} />); await flush(); });
  try {
    const loadedEstimate = () => renderer.root.findAllByProps({ className: 'estimate-strip' }).find(node => /model estimate/.test(renderedText(node)));
    assert.match(renderedText(loadedEstimate()), /3\.0 GB/);
    assert.match(renderedText(renderer.root.findByProps({ role: 'log', 'aria-label': 'Server logs' })), /Stable worker output/);
    const readCount = bridge.calls.length;
    const created = timers.created;
    const cleared = timers.cleared;
    deferReads = true;
    await act(async () => {
      renderer.update(<ServerView snapshot={{ ...structuredClone(initial), telemetry: { system: { cpu_percent: 18, ram_used_bytes: 1024 }, workers: [], managed_totals: {} } }} onAction={async () => ({})} onLibrary={() => {}} />);
      await flush();
    });
    assert.match(renderedText(renderer.root.findByProps({ role: 'log', 'aria-label': 'Server logs' })), /Stable worker output/, 'a telemetry render must not blank logs while an unnecessary request waits');
    assert.ok(loadedEstimate(), 'removing the estimate row can shrink the scroll container and clamp its position');
    assert.match(renderedText(loadedEstimate()), /3\.0 GB/);
    assert.equal(bridge.calls.length, readCount, 'equal profile/model data and new callback identity do not trigger new reads');
    assert.equal(timers.created, created, 'telemetry does not recreate the polling interval');
    assert.equal(timers.cleared, cleared, 'telemetry does not tear down the polling interval');
  } finally {
    await act(async () => renderer.unmount());
    pendingEstimate.resolve(estimate); pendingLogs.resolve(log);
  }
});

test('server logs and estimate remain rendered while genuine refresh requests are pending', async () => {
  const initialEstimate = { total_estimated_bytes: 3 * 1024 ** 3, budget_bytes: 8 * 1024 ** 3, complete: false };
  const initialLog = { text: 'Output before refresh', truncated: false };
  const pendingEstimate = deferred<any>();
  const pendingLogs = deferred<any>();
  let deferReads = false;
  const bridge = harness({
    'memory.estimate': () => deferReads ? pendingEstimate.promise : initialEstimate,
    'server.logs': () => deferReads ? pendingLogs.promise : initialLog,
  });
  const timers = controlledIntervals();
  const onAction = async () => ({});
  const initial = structuredClone(snapshot());
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={initial} onAction={onAction} onLibrary={() => {}} />); await flush(); });
  try {
    const estimateRow = () => renderer.root.findAllByProps({ className: 'estimate-strip' }).find(node => /model estimate/.test(renderedText(node)));
    deferReads = true;
    const changed = structuredClone(initial);
    changed.profiles[0].options.max_context = 16384;
    await act(async () => {
      renderer.update(<ServerView snapshot={changed} onAction={onAction} onLibrary={() => {}} />);
      timers.tick();
      await flush();
    });
    assert.ok(bridge.calls.filter(call => call.method === 'memory.estimate').length >= 2, 'a changed profile really requests a fresh estimate');
    assert.ok(bridge.calls.filter(call => call.method === 'server.logs').length >= 2, 'the scheduled poll really requests fresh logs');
    assert.ok(estimateRow(), 'keep the estimate row mounted while replacing its data');
    assert.match(renderedText(estimateRow()), /3\.0 GB/, 'retain the last estimate until its replacement arrives');
    assert.match(renderedText(renderer.root.findByProps({ role: 'log', 'aria-label': 'Server logs' })), /Output before refresh/);
    await act(async () => {
      pendingEstimate.resolve({ ...initialEstimate, total_estimated_bytes: 4 * 1024 ** 3 });
      pendingLogs.resolve({ text: 'Output after refresh', truncated: false });
      await flush();
    });
    assert.match(renderedText(estimateRow()), /4\.0 GB/);
    assert.match(renderedText(renderer.root.findByProps({ role: 'log', 'aria-label': 'Server logs' })), /Output after refresh/);
  } finally {
    await act(async () => renderer.unmount());
    pendingEstimate.resolve(initialEstimate); pendingLogs.resolve(initialLog);
  }
});

test('optional host and origin settings save empty lists and normalize explicit entries', async () => {
  const savedConfigs: any[] = [];
  const bridge = harness({
    'config.set': params => {
      const config = structuredClone(params.config);
      savedConfigs.push(config);
      bridge.data.config = config;
      return { config, worker_restart_required: true };
    },
  });
  bridge.data.config = {
    api: { host: '127.0.0.1', port: 8321, allowed_hosts: ['previous.local'], allowed_origins: ['https://previous.example'] },
    library_paths: [],
  };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    const hosts = () => renderer.root.findByProps({ 'aria-label': 'Allowed hosts' });
    const origins = () => renderer.root.findByProps({ 'aria-label': 'Allowed browser origins' });
    const save = () => renderer.root.findAllByType('button').find(button => renderedText(button).trim() === 'Save settings')!;
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Settings' }).props.onClick(); });
    assert.notEqual(hosts().props.required, true, 'hosts are optional in the rendered control');
    assert.notEqual(origins().props.required, true, 'browser origins are optional in the rendered control');
    assert.match(renderedText(hosts().parent), /blank.*any host/i, 'the host help explains the unrestricted empty-list policy');
    assert.match(renderedText(origins().parent), /blank.*any browser origin/i, 'the origin help explains the unrestricted empty-list policy');
    await act(async () => {
      hosts().props.onChange({ target: { value: '' } });
      origins().props.onChange({ target: { value: ' \n\t\n ' } });
    });
    await act(async () => { await save().props.onClick(); await flush(); });
    assert.equal(savedConfigs.length, 1, 'empty optional lists do not block saving');
    assert.deepEqual(savedConfigs[0].api.allowed_hosts, []);
    assert.deepEqual(savedConfigs[0].api.allowed_origins, []);
    assert.equal(renderer.root.findAllByProps({ className: 'inline-error' }).length, 0);
    assert.match(renderedText(renderer.root.findByProps({ className: 'warning-message', role: 'status' })), /restart.*model servers.*browser origins/i, 'a required worker restart remains explicit after saving optional lists');
    assert.equal(bridge.calls.some(call => ['server.start', 'server.stop'].includes(call.method)), false, 'saving policy does not automatically restart running workers');

    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Close dialog' }).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Settings' }).props.onClick(); });
    assert.equal(hosts().props.value, '', 'reopening the modal reads the saved empty hosts');
    assert.equal(origins().props.value, '', 'reopening the modal reads the saved empty origins');
    await act(async () => {
      hosts().props.onChange({ target: { value: ' studio.local \n\n 192.168.1.76 \n\t\n' } });
      origins().props.onChange({ target: { value: ' https://app.example \n\n http://192.168.1.88:3000 \n' } });
    });
    await act(async () => { await save().props.onClick(); await flush(); });
    assert.equal(savedConfigs.length, 2);
    assert.deepEqual(savedConfigs[1].api.allowed_hosts, ['studio.local', '192.168.1.76']);
    assert.deepEqual(savedConfigs[1].api.allowed_origins, ['https://app.example', 'http://192.168.1.88:3000']);
  } finally { await act(async () => renderer.unmount()); }
});

test('global stop remains available for an active API without running workers and for an enabled API with a bind error', async () => {
  harness();
  const data = { ...snapshot(), profiles: [], servers: [] };
  const calls: any[] = [];
  const onAction = async (method: string, params: any) => { calls.push({ method, params }); return {}; };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={data} onAction={onAction} onLibrary={() => {}} />); await flush(); });
  try {
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Stop server' }).props.disabled, false);
    assert.equal(renderer.root.findAllByType('button').some(button => renderedText(button).trim() === 'Stop all'), false);
    const failed = { ...data, api: { running: false, enabled: true, error: 'Address already in use' } };
    await act(async () => { renderer.update(<ServerView snapshot={failed} onAction={onAction} onLibrary={() => {}} />); });
    assert.match(renderedText(renderer.root.findByProps({ role: 'alert', className: 'inline-error' })), /Address already in use/);
    const stop = renderer.root.findByProps({ 'aria-label': 'Stop server' });
    assert.equal(stop.props.disabled, false, 'an enabled API can be stopped even when binding failed');
    await act(async () => { stop.props.onClick(); await flush(); });
    assert.deepEqual(calls, [{ method: 'server.stop', params: { all: true } }]);
  } finally { await act(async () => renderer.unmount()); }
});

test('an offline API starts without selected models and a pending global start can be stopped', async () => {
  const bridge = harness();
  const data = { ...snapshot(), api: { running: false, enabled: false }, profiles: [], servers: [] };
  const pendingStart = deferred<any>();
  const calls: any[] = [];
  const onAction = async (method: string, params: any) => { calls.push({ method, params }); return method === 'server.start' ? pendingStart.promise : {}; };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={data} onAction={onAction} onLibrary={() => {}} />); await flush(); });
  try {
    const start = renderer.root.findByProps({ 'aria-label': 'Start server' });
    assert.equal(start.props.disabled, false);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Stop server' }).props.disabled, true);
    const estimateReads = bridge.calls.filter(call => call.method === 'memory.estimate').length;
    await act(async () => { start.props.onClick(); await flush(); });
    assert.deepEqual(calls[0], { method: 'server.start', params: { all: true } });
    assert.equal(bridge.calls.filter(call => call.method === 'memory.estimate').length, estimateReads, 'starting only the API needs no model-memory confirmation');
    const stop = renderer.root.findByProps({ 'aria-label': 'Stop server' });
    assert.equal(stop.props.disabled, false, 'the global start is cancellable before any API or worker becomes ready');
    await act(async () => { stop.props.onClick(); await flush(); pendingStart.resolve({ cancelled: true }); await flush(); });
    assert.deepEqual(calls[1], { method: 'server.stop', params: { all: true } });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Start server' }).props.disabled, false);
    assert.equal(renderer.root.findAllByProps({ className: 'inline-error' }).length, 0);
  } finally { await act(async () => renderer.unmount()); pendingStart.resolve({ cancelled: true }); }
});

test('global stop also stops a loading worker when the inference API is offline', async () => {
  harness();
  const data = structuredClone(snapshot());
  data.api = { running: false, enabled: false };
  data.servers[0].state = 'starting';
  data.models.push({ ...model, id: 'm2' });
  data.profiles.push({ ...profile, id: 'p2', model_id: 'm2', alias: 'spare', selected: false });
  data.servers.push({ profile_id: 'p2', model_id: 'm2', alias: 'spare', state: 'stopped', active_requests: 0 });
  const pendingStop = deferred<any>();
  const calls: any[] = [];
  const onAction = async (method: string, params: any) => { calls.push({ method, params }); return pendingStop.promise; };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={data} onAction={onAction} onLibrary={() => {}} />); await flush(); });
  try {
    const stop = renderer.root.findByProps({ 'aria-label': 'Stop server' });
    assert.equal(stop.props.disabled, false);
    await act(async () => { stop.props.onClick(); await flush(); });
    assert.deepEqual(calls, [{ method: 'server.stop', params: { all: true } }]);
    assert.equal(renderer.root.findByProps({ className: 'start-server' }).props.disabled, true, 'a different stopped profile cannot start while global stop is pending');
    assert.equal(renderer.root.findByProps({ className: 'stop-server' }).props.disabled, true);
    data.servers[0].state = 'draining';
    await act(async () => { renderer.update(<ServerView snapshot={structuredClone(data)} onAction={onAction} onLibrary={() => {}} />); pendingStop.resolve({}); await flush(); });
    const draining = renderer.root.findByProps({ className: 'stop-server' });
    assert.match(renderedText(draining), /Stopping/);
    assert.equal(draining.props.disabled, true, 'a draining worker remains occupied after the global stop request resolves');
  } finally { await act(async () => renderer.unmount()); pendingStop.resolve({}); }
});

test('selected memory details distinguish complete model names, aliases, disk weights, current values and peaks', async () => {
  harness();
  const GIB = 1024 ** 3;
  const MIB = 1024 ** 2;
  const data = {
    ...snapshot(),
    models: [{ ...model, name: 'qwen3.5-0.8b-mlx-4bit', weights_bytes: 600 * MIB }, { ...model, id: 'm2', name: 'Qwen3.5-4B-MLX-4bit', weights_bytes: 3 * GIB }],
    profiles: [{ ...profile, alias: 'studio-small' }, { ...profile, id: 'p2', model_id: 'm2', alias: 'development-large' }],
    servers: [{ profile_id: 'p1', model_id: 'm1', alias: 'studio-small', state: 'ready', pid: 101, active_requests: 0 }, { profile_id: 'p2', model_id: 'm2', alias: 'development-large', state: 'ready', pid: 202, active_requests: 0 }],
    telemetry: { system: {}, managed_totals: {}, workers: [
      { profile_id: 'p1', pid: 101, rss_bytes: GIB, mlx_active_bytes: 512 * MIB, mlx_cache_bytes: 128 * MIB, mlx_peak_bytes: 2 * GIB, rss_peak_bytes: 2 * GIB },
      { profile_id: 'p2', pid: 202, rss_bytes: 2 * GIB, mlx_active_bytes: 2.5 * GIB, mlx_cache_bytes: .5 * GIB, mlx_peak_bytes: 4 * GIB, rss_peak_bytes: 5 * GIB },
    ] },
  };
  const onAction = async () => ({});
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={data} onAction={onAction} onLibrary={() => {}} />); await flush(); });
  try {
    const panel = () => renderer.root.findByProps({ 'aria-label': 'Selected model memory' });
    const reading = (label: string) => panel().findAllByProps({ className: 'worker-memory-reading' }).find(node => renderedText(node.findByType('dt')) === label)!.findAllByType('dd')[0];
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Server logs profile' }).props.onChange({ target: { value: 'p1' } }); await flush(); });
    assert.equal(renderedText(panel().findByType('h2')), 'Qwen3.5-0.8B-MLX-4bit');
    assert.match(renderedText(panel()), /Profile\s+studio-small/);
    assert.match(renderedText(panel().findByProps({ className: 'worker-weights' })), /Weights on disk\s+600\.0 MB/);
    assert.equal(renderedText(reading('MLX active')), '512.0 MB');
    assert.equal(renderedText(reading('Estimated releasable')), '1.0 GB', 'RSS and MLX must not be added');
    await act(async () => { renderer.root.findAllByProps({ className: 'server-name' }).find(node => /development-large/.test(renderedText(node)))!.props.onClick(); await flush(); });
    assert.equal(renderedText(panel().findByType('h2')), 'Qwen3.5-4B-MLX-4bit');
    assert.match(renderedText(panel()), /Profile\s+development-large/);
    assert.equal(renderedText(reading('MLX active')), '2.5 GB');
    assert.equal(renderedText(reading('Process RSS')), '2.0 GB');
    assert.equal(renderedText(reading('Estimated releasable')), '3.0 GB');
    assert.equal(renderedText(panel().findByType('summary')), 'Peaks since worker start');
    assert.equal(renderedText(reading('MLX peak')), '4.0 GB');
    assert.equal(renderedText(reading('Locals RSS peak')), '5.0 GB');
    const stopped = structuredClone(data);
    stopped.servers[1].state = 'stopped'; stopped.servers[1].pid = null;
    await act(async () => { renderer.update(<ServerView snapshot={stopped} onAction={onAction} onLibrary={() => {}} />); await flush(); });
    assert.equal(renderedText(panel().findByType('h2')), 'Qwen3.5-4B-MLX-4bit', 'stopping must not remove the selected model panel');
    for (const label of ['MLX active', 'MLX cache', 'Process RSS', 'Estimated releasable', 'MLX peak', 'Locals RSS peak']) assert.equal(renderedText(reading(label)), '—');
  } finally { await act(async () => renderer.unmount()); }
});

test('global stop cancels a deferred memory preflight before it can launch models or open a review', async () => {
  const preflight = deferred<any>();
  harness({ 'memory.estimate': params => params.all ? preflight.promise : { total_estimated_bytes: 0, budget_bytes: 100, complete: false } });
  const data = structuredClone(snapshot()); data.api = { running: false, enabled: false }; data.servers[0].state = 'stopped';
  const calls: any[] = [];
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={data} onAction={async (method, params) => { calls.push({ method, params }); return {}; }} onLibrary={() => {}} />); await flush(); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Start server' }).props.onClick(); await flush(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Stop server' }).props.disabled, false);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Stop server' }).props.onClick(); await flush(); preflight.resolve({ requires_override: true }); await flush(); });
    assert.deepEqual(calls, [{ method: 'server.stop', params: { all: true } }]);
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Review memory usage' }).length, 0);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Start server' }).props.disabled, false);
  } finally { await act(async () => renderer.unmount()); preflight.resolve({ requires_override: true }); }
});

test('global stop dismisses memory review without starting the reviewed models', async () => {
  harness({ 'memory.estimate': params => ({ total_estimated_bytes: 100, budget_bytes: 80, requires_override: !!params.all, warnings: [] }) });
  const data = structuredClone(snapshot()); data.api = { running: false, enabled: false }; data.servers[0].state = 'stopped';
  const calls: any[] = [];
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={data} onAction={async (method, params) => { calls.push({ method, params }); return {}; }} onLibrary={() => {}} />); await flush(); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Start server' }).props.onClick(); await flush(); });
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Review memory usage' }).length, 1);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Stop server and cancel memory review' }).props.onClick(); await flush(); });
    assert.deepEqual(calls, [{ method: 'server.stop', params: { all: true } }]);
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Review memory usage' }).length, 0);
  } finally { await act(async () => renderer.unmount()); }
});
