import React from 'react';
import assert from 'node:assert/strict';
import test from 'node:test';
import { readFile } from 'node:fs/promises';
import TestRenderer, { act } from 'react-test-renderer';
import App from '../src/App';
import LibraryView from '../src/views/LibraryView';
import ServerView from '../src/views/ServerView';
import ChatView from '../src/views/ChatView';
import SearchView from '../src/views/SearchView';
import { Markdown } from '../src/components';
import { EMPTY_SNAPSHOT } from '../src/types';
import { openExternal } from '../src/api';

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
          if (command === 'studio_events') { if (overrides.studio_events) return overrides.studio_events(args); eventHandler?.({ payload: { type: 'connected', data: {} } }); return; }
          if (command !== 'studio_request') return;
          const { method, params } = args;
          calls.push({ method, params });
          if (overrides[method]) return overrides[method](params);
          if (method === 'snapshot' || method === 'status') return structuredClone(data);
          if (method === 'conversations.save') { data.conversations = [...data.conversations.filter(item => item.id !== params.conversation.id), structuredClone(params.conversation)]; return params.conversation; }
          if (method === 'conversations.move') { const conversation = data.conversations.find(item => item.id === params.id); if (!conversation) throw new Error('Conversation not found'); conversation.folder_id = params.folder_id; conversation.updated_at = new Date().toISOString(); return structuredClone(conversation); }
          if (method === 'folders.save') { data.folders = [...data.folders.filter(item => item.id !== params.folder.id), structuredClone(params.folder)]; return params.folder; }
          if (method === 'folders.remove') { data.folders = data.folders.filter(item => item.id !== params.id); data.conversations = data.conversations.map(item => item.folder_id === params.id ? { ...item, folder_id: null } : item); return { removed: params.id }; }
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
    storage,
    emit(type: string, value: any) { eventHandler?.({ payload: { type, data: value } }); },
    get data() { return data; },
    resolveStart(value: any) { assert.ok(finishStart, 'chat.start reached the bridge'); finishStart(value); },
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((complete, fail) => { resolve = complete; reject = fail; });
  return { promise, resolve: (value: T) => resolve(value), reject };
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

function conversationDrag() {
  const data = new Map<string, string>();
  const dataTransfer = { get types() { return [...data.keys()]; }, effectAllowed: '', dropEffect: '', setData(type: string, value: string) { data.set(type, value); }, getData(type: string) { return data.get(type) ?? ''; } };
  return { dataTransfer, preventDefault() {}, stopPropagation() {}, relatedTarget: null, currentTarget: { contains() { return false; } } };
}
function conversationItem(root: TestRenderer.ReactTestInstance, title: string) {
  return root.findAllByType('button').find(node => String(node.props.className).split(' ').includes('conversation-item') && renderedText(node).includes(title))!;
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
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Collapse folder Projects' }).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'New conversation in Projects' }).props.onClick(); });
    assert.ok(renderer.root.findByProps({ 'aria-label': 'Collapse folder Projects' }), 'the new chat opens its target folder');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Conversation folder' }).props.value, id);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Message' }).props.value, '');
    assert.equal(bridge.calls.some(call => ['folders.remove', 'conversations.remove'].includes(call.method)), false, 'the plus action never deletes anything');
  } finally { await act(async () => renderer.unmount()); }
});

test('conversation drag moves between collapsed folders and unfiled without stale snapshots undoing it', async () => {
  const stale = deferred<any>(); let holdSnapshot = false;
  const bridge = harness({ snapshot: () => holdSnapshot ? stale.promise : structuredClone(bridge.data) });
  bridge.data.folders.push({ id: 'a', name: 'Writing' }, { id: 'b', name: 'Projects' });
  bridge.data.conversations.push({ id: 'chat', title: 'Move this chat', folder_id: 'a', model: 'test', messages: [{ role: 'user', content: 'Keep these messages' }] });
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  const target = (name: string) => renderer.root.findByProps({ role: 'group', 'aria-label': name });
  const dragTo = async (name: string) => {
    const event = conversationDrag();
    await act(async () => { conversationItem(renderer.root, 'Move this chat').props.onDragStart(event); });
    await act(async () => { target(name).props.onDragOver(event); });
    assert.match(target(name).props.className, /drop-active/);
    await act(async () => { target(name).props.onDrop(event); await flush(); });
  };
  try {
    await act(async () => { conversationItem(renderer.root, 'Move this chat').props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: 'Keep my draft' } }); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Collapse folder Projects' }).props.onClick(); });
    const oldSnapshot = structuredClone(bridge.data); holdSnapshot = true;
    await act(async () => { bridge.emit('folders.changed', {}); await flush(); });
    await dragTo('Conversation folder Projects');
    assert.deepEqual(bridge.calls.filter(call => call.method === 'conversations.move').map(call => call.params), [{ id: 'chat', folder_id: 'b' }]);
    assert.ok(renderer.root.findByProps({ 'aria-label': 'Collapse folder Projects' }), 'dropping into a collapsed folder expands it');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Conversation folder' }).props.value, 'b');
    holdSnapshot = false;
    await act(async () => { stale.resolve(oldSnapshot); await flush(); });
    assert.ok(conversationItem(target('Conversation folder Projects'), 'Move this chat'), 'an earlier snapshot must not undo the move');
    await dragTo('Conversations without a folder');
    assert.equal(bridge.data.conversations[0].folder_id, null);
    await dragTo('Conversation folder Writing');
    assert.equal(bridge.data.conversations[0].folder_id, 'a');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Message' }).props.value, 'Keep my draft');
    assert.deepEqual(bridge.data.conversations[0].messages, [{ role: 'user', content: 'Keep these messages' }]);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'My models' }).props.onClick(); renderer.root.findByProps({ 'aria-label': 'Chat' }).props.onClick(); });
    assert.ok(conversationItem(target('Conversation folder Writing'), 'Move this chat'));
  } finally { await act(async () => renderer.unmount()); }
});

test('conversation drops ignore unrelated drags and duplicate moves and preserve the source on failure', async () => {
  const moving = deferred<any>();
  const bridge = harness({ 'conversations.move': () => moving.promise });
  bridge.data.folders.push({ id: 'a', name: 'Writing' }, { id: 'b', name: 'Projects' });
  bridge.data.conversations.push({ id: 'chat', title: 'Keep this chat', folder_id: 'a', messages: [{ role: 'user', content: 'Keep me' }] });
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  const target = () => renderer.root.findByProps({ role: 'group', 'aria-label': 'Conversation folder Projects' });
  try {
    await act(async () => { conversationItem(renderer.root, 'Keep this chat').props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: 'Do not send during the move' } }); });
    const foreign = conversationDrag(); foreign.dataTransfer.setData('text/plain', 'chat');
    await act(async () => { target().props.onDragOver(foreign); target().props.onDrop(foreign); });
    assert.equal(bridge.calls.filter(call => call.method === 'conversations.move').length, 0);
    const same = conversationDrag();
    await act(async () => { conversationItem(renderer.root, 'Keep this chat').props.onDragStart(same); });
    await act(async () => { renderer.root.findByProps({ role: 'group', 'aria-label': 'Conversation folder Writing' }).props.onDrop(same); });
    assert.equal(bridge.calls.filter(call => call.method === 'conversations.move').length, 0);
    const event = conversationDrag();
    await act(async () => { conversationItem(renderer.root, 'Keep this chat').props.onDragStart(event); });
    await act(async () => { target().props.onDrop(event); await flush(); });
    assert.equal(conversationItem(renderer.root, 'Keep this chat').props.draggable, false);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Send message' }).props.disabled, true);
    assert.equal(renderer.root.findByProps({ className: 'conversation-title' }).findByType('button').props.disabled, true);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Send message' }).props.onClick(); await flush(); });
    assert.equal(bridge.calls.some(call => call.method === 'chat.start'), false, 'a full save cannot race an in-flight folder move');
    await act(async () => { target().props.onDrop(event); await flush(); });
    assert.equal(bridge.calls.filter(call => call.method === 'conversations.move').length, 1);
    await act(async () => { moving.reject(new Error('Could not move conversation')); await flush(); });
    assert.equal(bridge.data.conversations[0].folder_id, 'a');
    assert.ok(conversationItem(renderer.root.findByProps({ role: 'group', 'aria-label': 'Conversation folder Writing' }), 'Keep this chat'));
    assert.match(renderedText(renderer.root), /Could not move conversation/);
    assert.equal(conversationItem(renderer.root, 'Keep this chat').props.draggable, true);
    assert.equal(bridge.calls.some(call => ['conversations.save', 'conversations.remove', 'folders.remove'].includes(call.method)), false);
  } finally { await act(async () => renderer.unmount()); }
});

test('a pending title save blocks folder moves even after the title input loses focus', async () => {
  const saved = deferred<void>();
  const bridge = harness({ 'conversations.save': async ({ conversation }) => { await saved.promise; bridge.data.conversations[0] = structuredClone(conversation); return conversation; } });
  bridge.data.folders.push({ id: 'a', name: 'Writing' }, { id: 'b', name: 'Projects' });
  bridge.data.conversations.push({ id: 'chat', title: 'Rename this chat', folder_id: 'a', model: 'test', messages: [{ role: 'user', content: 'Keep me' }] });
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { conversationItem(renderer.root, 'Rename this chat').props.onClick(); });
    await act(async () => { renderer.root.findByProps({ className: 'conversation-title' }).findByType('button').props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Conversation title' }).props.onChange({ target: { value: 'Renamed safely' } }); });
    await act(async () => { renderer.root.findByProps({ className: 'conversation-title' }).findByType('form').props.onSubmit({ preventDefault() {} }); await flush(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Conversation title' }).props.onBlur(); });
    assert.equal(conversationItem(renderer.root, 'Rename this chat').props.draggable, false);
    const drag = conversationDrag();
    await act(async () => { conversationItem(renderer.root, 'Rename this chat').props.onDragStart(drag); });
    await act(async () => { renderer.root.findByProps({ role: 'group', 'aria-label': 'Conversation folder Projects' }).props.onDrop(drag); await flush(); });
    assert.equal(bridge.calls.some(call => call.method === 'conversations.move'), false);
    await act(async () => { saved.resolve(); await flush(); });
    assert.equal(bridge.data.conversations[0].title, 'Renamed safely');
    assert.equal(conversationItem(renderer.root, 'Renamed safely').props.draggable, true);
    const move = conversationDrag();
    await act(async () => { conversationItem(renderer.root, 'Renamed safely').props.onDragStart(move); });
    await act(async () => { renderer.root.findByProps({ role: 'group', 'aria-label': 'Conversation folder Projects' }).props.onDrop(move); await flush(); });
    assert.equal(bridge.data.conversations[0].folder_id, 'b');
    assert.equal(bridge.data.conversations[0].title, 'Renamed safely');
  } finally { await act(async () => renderer.unmount()); }
});

test('moving a streaming conversation preserves the live answer, model and composer draft', async () => {
  const bridge = harness(); bridge.data.folders.push({ id: 'b', name: 'Projects' });
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: 'A streaming request' } }); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Send message' }).props.onClick(); await flush(); });
    const id = bridge.data.conversations[0].id;
    await act(async () => { bridge.resolveStart({ request_id: 'stream', conversation_id: id }); await flush(); });
    await act(async () => { bridge.emit('chat.delta', { request_id: 'stream', conversation_id: id, delta: 'Keep this partial answer' }); await flush(); renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: 'Keep my next question' } }); });
    const event = conversationDrag();
    await act(async () => { conversationItem(renderer.root, 'A streaming request').props.onDragStart(event); });
    await act(async () => { renderer.root.findByProps({ role: 'group', 'aria-label': 'Conversation folder Projects' }).props.onDrop(event); await flush(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Conversation folder' }).props.value, 'b');
    assert.ok(renderer.root.findAllByType(Markdown).some(node => node.props.children === 'Keep this partial answer'));
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Message' }).props.value, 'Keep my next question');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Chat model' }).props.value, 'test');
    const completed = { role: 'assistant', content: 'Complete answer' };
    bridge.data.conversations[0].messages.push(completed);
    await act(async () => { bridge.emit('chat.done', { request_id: 'stream', conversation_id: id, message: completed }); await flush(); });
    assert.equal(bridge.data.conversations[0].folder_id, 'b');
    assert.ok(renderer.root.findAllByType(Markdown).some(node => node.props.children === 'Complete answer'));
  } finally { await act(async () => renderer.unmount()); }
});

test('pointer dragging uses a threshold, cancels native export and commits only a valid completed drop', async () => {
  const bridge = harness();
  bridge.data.folders.push({ id: 'a', name: 'Writing' }, { id: 'b', name: 'Projects' });
  bridge.data.conversations.push({ id: 'chat', title: 'Pointer chat', folder_id: 'a', messages: [{ role: 'user', content: 'Keep me' }] });
  const dropElement = { dataset: { conversationDropTarget: 'folder', folderId: 'b' } };
  (document as any).elementFromPoint = () => ({ closest: () => dropElement });
  let captured = 0; let released = 0;
  const element = { setPointerCapture() { captured++; }, hasPointerCapture() { return true; }, releasePointerCapture() { released++; } };
  const point = (x: number, y: number) => ({ pointerId: 1, pointerType: 'mouse', isPrimary: true, button: 0, clientX: x, clientY: y, currentTarget: element, preventDefault() {}, stopPropagation() {} });
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  const row = () => conversationItem(renderer.root, 'Pointer chat');
  const moves = () => bridge.calls.filter(call => call.method === 'conversations.move');
  try {
    await act(async () => { row().props.onPointerDown(point(10, 10)); row().props.onPointerMove(point(12, 12)); row().props.onPointerUp(point(12, 12)); });
    assert.equal(moves().length, 0, 'ordinary clicks do not move a conversation');
    await act(async () => { row().props.onPointerDown(point(10, 10)); row().props.onPointerMove(point(40, 40)); });
    assert.match(row().props.className, /dragging/);
    let nativePrevented = false;
    await act(async () => { row().props.onDragStart({ ...conversationDrag(), preventDefault() { nativePrevented = true; } }); });
    assert.equal(nativePrevented, true, 'the OS drag loop must not consume an internal pointer gesture');
    await act(async () => { row().props.onPointerCancel(point(40, 40)); });
    assert.equal(moves().length, 0);
    assert.doesNotMatch(row().props.className, /dragging/);
    await act(async () => { row().props.onPointerDown(point(10, 10)); row().props.onPointerMove(point(40, 40)); });
    await act(async () => { const source = row(); source.props.onPointerUp(point(40, 40)); source.props.onClick({ preventDefault() {}, stopPropagation() {} }); await flush(); });
    assert.deepEqual(moves().map(call => call.params), [{ id: 'chat', folder_id: 'b' }]);
    assert.equal(bridge.data.conversations[0].folder_id, 'b');
    await act(async () => { row().props.onPointerUp(point(40, 40)); });
    assert.equal(moves().length, 1, 'pointer-up cannot commit a second move');
    assert.equal(captured, released);
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

test('settings show a stored configuration bind failure and allow a successful save retry', async () => {
  const bindError = 'Cannot bind Studio API to 127.0.0.1:8333; another server may already use this port';
  let saves = 0;
  const bridge = harness({
    'config.set': params => {
      saves += 1;
      const config = structuredClone(params.config);
      const api = saves === 2 ? { running: false, enabled: true, error: bindError } : { running: true, enabled: true, error: null };
      bridge.data.config = config;
      bridge.data.api = api;
      return { config, api, worker_restart_required: saves > 1 };
    },
  });
  bridge.data.config = {
    api: { enabled: true, host: '127.0.0.1', port: 8321, allowed_hosts: [], allowed_origins: [] },
    library_paths: [], desktop: { menu_bar_enabled: false },
  };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Settings' }).props.onClick(); });
    const panel = () => renderer.root.findByProps({ id: 'settings-general-panel' });
    const footer = () => panel().findByProps({ className: 'modal-footer' });
    const save = () => footer().findAllByType('button').find(button => renderedText(button).trim() === 'Save settings')!;
    const port = () => panel().findByProps({ type: 'number' });
    await act(async () => { await save().props.onClick(); await flush(); });
    assert.match(renderedText(footer()), /Settings saved/, 'a healthy API save reports success');

    await act(async () => { port().props.onChange({ target: { value: '8333' } }); });
    await act(async () => { await save().props.onClick(); await flush(); });
    assert.match(renderedText(panel().findByProps({ role: 'alert', className: 'inline-error' })), /Settings were saved, but the inference API is unavailable/);
    assert.match(renderedText(panel()), /Cannot bind Studio API to 127\.0\.0\.1:8333/);
    assert.doesNotMatch(renderedText(footer()), /Settings saved/, 'a bind failure clears the previous success indicator');
    assert.equal(port().props.value, '8333', 'the form remains open with its saved port');
    assert.equal(save().props.disabled, false, 'the form can be saved again');
    assert.match(renderedText(panel().findByProps({ className: 'warning-message', role: 'status' })), /restart.*model servers.*browser origins/i, 'the worker restart notice survives an API bind failure');

    await act(async () => { port().props.onChange({ target: { value: '8344' } }); });
    await act(async () => { await save().props.onClick(); await flush(); });
    assert.equal(saves, 3);
    assert.equal(panel().findAllByProps({ className: 'inline-error' }).length, 0, 'a healthy retry clears the bind failure');
    assert.match(renderedText(footer()), /Settings saved/);
    assert.equal(bridge.calls.some(call => ['server.start', 'server.stop'].includes(call.method)), false, 'saving does not change worker lifecycle');
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

test('menu bar preference defaults off and persists without changing server or network state', async () => {
  const savedConfigs: any[] = [];
  const bridge = harness({
    'config.set': params => {
      const config = structuredClone(params.config);
      savedConfigs.push(config);
      bridge.data.config = config;
      return { config, worker_restart_required: false };
    },
  });
  bridge.data.config = {
    api: { enabled: true, host: '127.0.0.1', port: 8321, allowed_hosts: [], allowed_origins: [] },
    desktop: { future_preference: 'keep' },
    library_paths: ['/models/extra'], memory_budget_bytes: 8 * 1024 ** 3,
  };
  const original = structuredClone(bridge.data.config);
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    const toggle = () => renderer.root.findByProps({ 'aria-label': 'Show menu bar icon' });
    const save = () => renderer.root.findAllByType('button').find(button => renderedText(button).trim() === 'Save settings')!;
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Settings' }).props.onClick(); });
    assert.equal(toggle().props.checked, false, 'old configurations do not create an icon automatically');
    await act(async () => { toggle().props.onChange({ target: { checked: true } }); });
    await act(async () => { await save().props.onClick(); await flush(); });
    assert.deepEqual(savedConfigs[0], { ...original, desktop: { ...original.desktop, menu_bar_enabled: true } });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Close dialog' }).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Settings' }).props.onClick(); });
    assert.equal(toggle().props.checked, true, 'reopening reads the persisted setting');
    await act(async () => { toggle().props.onChange({ target: { checked: false } }); });
    await act(async () => { await save().props.onClick(); await flush(); });
    assert.equal(savedConfigs[1].desktop.menu_bar_enabled, false);
    assert.equal(bridge.calls.some(call => ['server.start', 'server.stop'].includes(call.method)), false, 'the preference does not change inference state');
  } finally { await act(async () => renderer.unmount()); }
});

test('About is available from settings and a native menu event with the bundled license and Armateur Labs link', async () => {
  const bridge = harness();
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Settings' }).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ role: 'tab', 'aria-label': 'About Locals Studio' }).props.onClick(); });
    const about = () => renderer.root.findByProps({ role: 'dialog', 'aria-label': 'About Locals Studio' });
    assert.match(renderedText(about()), /0\.1\.0/);
    const license = await readFile(new URL('../LICENSE', import.meta.url), 'utf8');
    assert.equal(renderedText(about().findByProps({ 'aria-label': 'MIT License text' })).trim(), license.trim(), 'the offline license matches the repository');
    assert.equal(about().findByProps({ 'aria-label': 'Visit Armateur Labs' }).props.href, 'https://armateurlabs.fr');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Close dialog' }).props.onClick(); });
    await act(async () => { bridge.emit('desktop.about', {}); await flush(); });
    assert.ok(about(), 'the native About action opens the same section');
    assert.equal(bridge.calls.some(call => ['server.start', 'server.stop', 'config.set'].includes(call.method)), false);
  } finally { await act(async () => renderer.unmount()); }
});

test('external links allow the three HTTPS destinations and reject credentials and lookalike hosts before native invocation', async () => {
  harness();
  const opened: string[] = [];
  (window as any).__TAURI_INTERNALS__.invoke = async (command: string, args: any) => {
    assert.equal(command, 'open_external'); opened.push(args.url);
  };
  await openExternal('https://armateurlabs.fr');
  await openExternal('https://huggingface.co/mlx-community/Test');
  await openExternal('https://generatedcomputers.com/');
  for (const url of ['http://armateurlabs.fr', 'https://armateurlabs.fr.evil.example', 'https://evil.example@armateurlabs.fr', 'https://user:secret@huggingface.co/model', 'https://www.armateurlabs.fr', 'http://generatedcomputers.com/', 'https://generatedcomputers.com.evil.example/', 'https://www.generatedcomputers.com/', 'https://other.generatedcomputers.com/', 'https://user@generatedcomputers.com/', 'https://user:secret@generatedcomputers.com/', 'javascript:alert(1)']) {
    await assert.rejects(openExternal(url));
  }
  assert.deepEqual(opened, ['https://armateurlabs.fr', 'https://huggingface.co/mlx-community/Test', 'https://generatedcomputers.com/']);
});

function chatAvailabilitySnapshot() {
  const data = snapshot();
  const stopped = { ...profile, id: 'stopped-profile', alias: 'a-stopped', model_id: 'stopped-model' };
  return {
    ...data,
    models: [...data.models, { ...model, id: stopped.model_id }],
    profiles: [stopped, profile],
    servers: [{ profile_id: stopped.id, model_id: stopped.model_id, alias: stopped.alias, state: 'stopped', active_requests: 0 }, ...data.servers],
  };
}

test('chat defaults to a ready model and Send submits even when an earlier profile is stopped', async () => {
  harness();
  const data = chatAvailabilitySnapshot();
  const sent: any[] = [];
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ChatView active snapshot={data} streams={{}} errors={{}} onSend={async (...args) => { sent.push(args); }} onSave={async () => {}} onAction={async () => ({})} onServer={() => {}} />); await flush(); });
  try {
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Chat model' }).props.value, 'test');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: 'Hello' } }); });
    const send = renderer.root.findByProps({ 'aria-label': 'Send message' });
    assert.equal(send.props.disabled, false);
    await act(async () => { send.props.onClick(); await flush(); });
    assert.equal(sent.length, 1);
    assert.equal(sent[0][1], 'Hello');
    assert.equal(sent[0][2], 'test');
    const withLegacyWorker = { ...data, servers: [{ ...data.servers[1], profile_id: 'removed-profile', state: 'stopped' }, ...data.servers] };
    await act(async () => { renderer.update(<ChatView active snapshot={withLegacyWorker} streams={{}} errors={{}} onSend={async (...args) => { sent.push(args); }} onSave={async () => {}} onAction={async () => ({})} onServer={() => {}} />); await flush(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: 'Continue' } }); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Send message' }).props.disabled, false, 'a stopped legacy worker must not hide the ready worker for the current profile');
  } finally { await act(async () => renderer.unmount()); }
});

test('automatic chat choice follows a newly ready model while manual choices and restored conversations are retained', async () => {
  harness();
  const offline = chatAvailabilitySnapshot();
  offline.servers = offline.servers.map(server => ({ ...server, state: 'stopped' }));
  const ready = chatAvailabilitySnapshot();
  const callbacks = { onSend: async () => {}, onSave: async () => {}, onAction: async () => ({}), onServer: () => {} };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ChatView active snapshot={offline} streams={{}} errors={{}} {...callbacks} />); await flush(); });
  try {
    const picker = () => renderer.root.findByProps({ 'aria-label': 'Chat model' });
    const input = () => renderer.root.findByProps({ 'aria-label': 'Message' });
    await act(async () => { input().props.onChange({ target: { value: 'Keep this draft' } }); });
    await act(async () => { renderer.update(<ChatView active snapshot={ready} streams={{}} errors={{}} {...callbacks} />); await flush(); });
    assert.equal(picker().props.value, 'test', 'an automatic choice can follow a worker that became ready');
    assert.equal(input().props.value, 'Keep this draft');
    await act(async () => { picker().props.onChange({ target: { value: 'a-stopped' } }); });
    await act(async () => { renderer.update(<ChatView active snapshot={structuredClone(ready)} streams={{}} errors={{}} {...callbacks} />); await flush(); });
    assert.equal(picker().props.value, 'a-stopped', 'polling must preserve an explicit model choice');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Send message' }).props.disabled, true);
    assert.ok(picker().findAllByType('option').some(option => option.props.value === 'a-stopped' && /stopped/i.test(renderedText(option))));
    await act(async () => { renderer.root.findByProps({ className: 'new-chat' }).props.onClick(); await flush(); });
    assert.equal(picker().props.value, 'test', 'a new conversation starts with a ready model');
    const withHistory = { ...ready, conversations: [{ id: 'saved-stopped-chat', title: 'Previous conversation', model: 'a-stopped', messages: [{ role: 'user', content: 'Earlier prompt' }], folder_id: null }] };
    await act(async () => { renderer.update(<ChatView active snapshot={withHistory} streams={{}} errors={{}} {...callbacks} />); await flush(); });
    await act(async () => { conversationItem(renderer.root, 'Previous conversation').props.onClick(); await flush(); });
    assert.equal(picker().props.value, 'a-stopped', 'restoring a conversation retains its model');
    await act(async () => { renderer.update(<ChatView active snapshot={structuredClone(withHistory)} streams={{}} errors={{}} {...callbacks} />); await flush(); });
    assert.equal(picker().props.value, 'a-stopped');
  } finally { await act(async () => renderer.unmount()); }
});

test('appearance changes apply immediately, survive reopening, and do not mutate server configuration', async () => {
  const bridge = harness();
  bridge.storage.set('studio.theme', 'light');
  bridge.storage.set('studio.palette', 'invalid-palette');
  bridge.storage.set('studio.textSize', 'invalid-size');
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    assert.deepEqual(document.documentElement.dataset, { theme: 'light', palette: 'green', textSize: 'standard' });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Settings' }).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Sapphire palette' }).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Text size' }).props.onChange({ target: { value: 'large' } }); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Color mode' }).props.onChange({ target: { value: 'dark' } }); });
    assert.deepEqual(document.documentElement.dataset, { theme: 'dark', palette: 'sapphire', textSize: 'large' });
    assert.equal(bridge.storage.get('studio.palette'), 'sapphire');
    assert.equal(bridge.storage.get('studio.textSize'), 'large');
    assert.equal(bridge.calls.some(call => ['config.set', 'server.start', 'server.stop'].includes(call.method)), false);
    await act(async () => { renderer.unmount(); renderer = TestRenderer.create(<App />); await flush(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Settings' }).props.onClick(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Sapphire palette' }).props['aria-pressed'], true);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Text size' }).props.value, 'large');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Color mode' }).props.value, 'dark');
  } finally { await act(async () => renderer.unmount()); }
});

test('server focus and logs selection survive navigating to another view', async () => {
  const bridge = harness();
  bridge.data.models.push({ ...model, id: 'm2', name: 'Second model' });
  bridge.data.profiles.push({ ...profile, id: 'p2', model_id: 'm2', alias: 'second' });
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Model server' }).props.onClick(); await flush(); });
    await act(async () => { renderer.root.findAllByProps({ className: 'server-name' }).find(node => /second/.test(renderedText(node)))!.props.onClick(); await flush(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Server logs profile' }).props.value, 'p2');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Chat' }).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Model server' }).props.onClick(); await flush(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Server logs profile' }).props.value, 'p2');
    assert.match(renderedText(renderer.root.findByProps({ 'aria-label': 'Selected model memory' })), /Second model/);
    assert.equal(bridge.data.profiles.every(item => item.selected), true, 'navigation does not alter the persisted serving selection');
  } finally { await act(async () => renderer.unmount()); }
});

test('chat model picker is inside the composer below the message field', async () => {
  harness();
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ChatView active snapshot={snapshot()} streams={{}} errors={{}} onSend={async () => {}} onSave={async () => {}} onAction={async () => ({})} onServer={() => {}} />); await flush(); });
  try {
    const composer = renderer.root.findByProps({ className: 'composer' });
    assert.equal(composer.findAllByProps({ 'aria-label': 'Chat model' }).length, 1);
    assert.equal(renderer.root.findAllByProps({ className: 'chat-toolbar' }).length, 0);
    const controls = composer.findByProps({ className: 'composer-bottom' });
    assert.equal(controls.findAllByProps({ 'aria-label': 'Chat model' }).length, 1);
    assert.ok(composer.children.indexOf(composer.findByProps({ 'aria-label': 'Message' })) < composer.children.indexOf(controls));
  } finally { await act(async () => renderer.unmount()); }
});

test('Discover enriches sizes with bounded requests, revision pinning, isolated failures and a shared cache', async () => {
  const sha = '1234567890abcdef1234567890abcdef12345678';
  const results = Array.from({ length: 5 }, (_, index) => ({ id: `ui-size-test/model-${index}`, sha, tags: ['mlx'], createdAt: '2026-10-25T00:00:00Z', studio_compatibility: { status: 'compatible', reason: null }, studio_size: { revision: sha, weights_bytes: null, weights_complete: false } }));
  const requests = new Map<string, ReturnType<typeof deferred<any>>>();
  const bridge = harness({ search: () => results, 'model.info': params => { const value = deferred<any>(); requests.set(params.repo_id, value); return value.promise; } });
  const tasks = new Map<number, () => void>();
  let nextTimer = 0;
  window.setTimeout = ((callback: () => void) => { tasks.set(++nextTimer, callback); return nextTimer; }) as typeof window.setTimeout;
  window.clearTimeout = ((id: number) => { tasks.delete(id); }) as typeof window.clearTimeout;
  const callbacks = { onAction: async () => ({}) };
  let renderer: TestRenderer.ReactTestRenderer;
  const searchNow = async () => { await flush(); const pending = [...tasks.values()]; tasks.clear(); for (const run of pending) run(); await flush(); };
  const info = (id: string) => ({ id, sha, siblings: [{ rfilename: 'model.safetensors', size: 1_250_000_000 }], createdAt: '2026-10-25T00:00:00Z', studio_compatibility: { status: 'compatible', reason: null }, studio_size: { revision: sha, weights_bytes: 1_250_000_000, weights_complete: true, download_bytes: 1_300_000_000, download_complete: true } });
  await act(async () => { renderer = TestRenderer.create(<SearchView snapshot={snapshot()} {...callbacks} />); await searchNow(); });
  try {
    assert.equal(requests.size, 3, 'metadata requests are bounded while cards already render');
    assert.equal(renderer.root.findAllByProps({ className: 'discover-card' }).length, 5);
    await act(async () => { requests.get(results[0].id)!.resolve(info(results[0].id)); await flush(); });
    assert.equal(requests.size, 4);
    assert.match(renderedText(renderer.root.findByProps({ 'aria-label': `Download size for ${results[0].id}` })), /1\.30 GB/);
    await act(async () => { requests.get(results[1].id)!.reject(new Error('Metadata temporarily unavailable')); await flush(); });
    assert.equal(requests.size, 5);
    assert.match(renderedText(renderer.root.findByProps({ 'aria-label': `Download size for ${results[1].id}` })), /Size unavailable/);
    await act(async () => { for (const result of results.slice(2)) requests.get(result.id)!.resolve(info(result.id)); await flush(); });
    const calls = bridge.calls.filter(call => call.method === 'model.info');
    assert.equal(calls.length, 5);
    assert.ok(calls.every(call => call.params.revision === sha));
    await act(async () => { renderer.unmount(); renderer = TestRenderer.create(<SearchView snapshot={snapshot()} {...callbacks} />); await searchNow(); });
    assert.equal(bridge.calls.filter(call => call.method === 'model.info').length, 5, 'revisiting reuses successful and failed metadata without another burst');
    assert.match(renderedText(renderer.root.findByProps({ 'aria-label': `Download size for ${results[0].id}` })), /1\.30 GB/);
  } finally {
    await act(async () => { renderer.unmount(); for (const [id, value] of requests) value.resolve(info(id)); await flush(); });
  }
});

test('Discover sortable headers and grid controls preserve ordering and preferences without another Hub request', async () => {
  const query = 'ui-catalogue-navigation';
  const sized = (id: string, downloads: number, weights: number, disk: number) => ({ id, downloads, sha: 'a'.repeat(40), createdAt: '2026-10-25T00:00:00Z', studio_compatibility: { status: 'compatible', reason: null }, studio_size: { revision: 'a'.repeat(40), weights_bytes: weights, weights_complete: true, download_bytes: disk, download_complete: true } });
  const results = [sized('catalogue-test/large-37B-4bit', 2, 3e9, 4e9), sized('catalogue-test/small-2B-8bit', 9, 1e9, 5e9), { id: 'catalogue-test/unknown', downloads: 0, createdAt: '2026-10-25T00:00:00Z', studio_compatibility: { status: 'compatible', reason: null } }];
  const bridge = harness({ search: () => results, 'model.info': () => { throw new Error('No metadata available'); } });
  bridge.storage.set('studio.discover.preferences', JSON.stringify({ query, view: 'grid', sort: 'downloads', direction: 'desc' }));
  const tasks = new Map<number, () => void>();
  let timer = 0;
  window.setTimeout = ((callback: () => void) => { tasks.set(++timer, callback); return timer; }) as typeof window.setTimeout;
  window.clearTimeout = ((id: number) => { tasks.delete(id); }) as typeof window.clearTimeout;
  const runTimers = async () => { await flush(); const pending = [...tasks.values()]; tasks.clear(); for (const run of pending) run(); await flush(); };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<SearchView snapshot={snapshot()} onAction={async () => ({})} />); await runTimers(); });
  try {
    const rows = () => renderer.root.findAllByProps({ className: 'discover-list-row' }).map(row => row.props['aria-label']);
    const sortButton = (label: string) => renderer.root.findByProps({ 'aria-label': `Sort by ${label}` });
    const sortButtons = () => renderer.root.findAllByType('button').filter(button => typeof button.props['data-sort'] === 'string');
    const assertSort = (key: string, label: string, direction: 'ascending' | 'descending', inList = true) => {
      assert.deepEqual(sortButtons().map(button => button.props['data-sort']), ['downloads', 'bits', 'parameters', 'disk', 'published']);
      assert.equal(sortButton(label).props['aria-pressed'], true);
      assert.equal(sortButtons().filter(button => button.props['aria-pressed'] === true).length, 1);
      const arrows = sortButton(label).findAllByType('svg');
      assert.equal(arrows.length, 1);
      assert.ok(arrows[0].props['aria-hidden'] === true || arrows[0].props['aria-hidden'] === 'true');
      assert.equal(arrows[0].props.width, 12);
      assert.equal(arrows[0].props.height, 12);
      if (inList) {
        const table = renderer.root.findByProps({ role: 'table', 'aria-label': 'Model catalogue' });
        const heading = table.findByProps({ className: 'discover-list-heading' });
        assert.equal(heading.props.role, 'row');
        const columns = heading.findAllByProps({ role: 'columnheader' }).filter(column => column.findAllByType('button').some(button => button.props['data-sort']));
        assert.equal(columns.length, 5);
        for (const column of columns) {
          const button = column.findAllByType('button').find(button => button.props['data-sort'])!;
          assert.equal(column.props['aria-sort'], button.props['data-sort'] === key ? direction : 'none');
        }
        for (const row of table.findAllByProps({ className: 'discover-list-row' })) {
          assert.equal(row.props.role, 'row');
          assert.equal(row.findAllByProps({ role: 'cell' }).length, 7);
        }
      }
    };
    const controls = renderer.root.findByProps({ className: 'catalogue-controls' });
    assert.equal(renderer.root.findByProps({ role: 'group', 'aria-label': 'Model families' }).parent, controls);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Refresh catalogue' }).parent, controls);
    assert.equal(renderer.root.findByProps({ role: 'group', 'aria-label': 'Catalogue view' }).parent, controls);
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Sort models by' }).length, 0);
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Sort ascending' }).length, 0);
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Sort descending' }).length, 0);
    assert.equal(renderer.root.findAllByProps({ className: 'discover-list-heading' }).length, 0);
    assert.ok(renderer.root.findByProps({ className: 'catalogue-grid-sort', role: 'group' }).findAllByType('button').includes(sortButton('Downloads')));
    assertSort('downloads', 'Downloads', 'descending', false);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'List view' }).props.onClick(); });
    assert.deepEqual(rows(), ['catalogue-test/small-2B-8bit', 'catalogue-test/large-37B-4bit', 'catalogue-test/unknown']);
    assertSort('downloads', 'Downloads', 'descending');
    await act(async () => { sortButton('Downloads').props.onClick(); });
    assertSort('downloads', 'Downloads', 'ascending');
    assert.deepEqual(rows(), ['catalogue-test/unknown', 'catalogue-test/large-37B-4bit', 'catalogue-test/small-2B-8bit']);
    await act(async () => { sortButton('Parameters').props.onClick(); await flush(); });
    assertSort('parameters', 'Parameters', 'descending');
    assert.deepEqual(rows(), ['catalogue-test/large-37B-4bit', 'catalogue-test/small-2B-8bit', 'catalogue-test/unknown']);
    await act(async () => { sortButton('Parameters').props.onClick(); });
    assertSort('parameters', 'Parameters', 'ascending');
    assert.deepEqual(rows(), ['catalogue-test/small-2B-8bit', 'catalogue-test/large-37B-4bit', 'catalogue-test/unknown']);
    await act(async () => { sortButton('Download size').props.onClick(); });
    assertSort('disk', 'Download size', 'descending');
    assert.deepEqual(rows(), ['catalogue-test/small-2B-8bit', 'catalogue-test/large-37B-4bit', 'catalogue-test/unknown']);
    await act(async () => { sortButton('Download size').props.onClick(); });
    assertSort('disk', 'Download size', 'ascending');
    assert.deepEqual(rows(), ['catalogue-test/large-37B-4bit', 'catalogue-test/small-2B-8bit', 'catalogue-test/unknown']);
    await act(async () => { sortButton('Bits').props.onClick(); });
    assertSort('bits', 'Bits', 'descending');
    assert.deepEqual(rows(), ['catalogue-test/small-2B-8bit', 'catalogue-test/large-37B-4bit', 'catalogue-test/unknown']);
    await act(async () => { sortButton('Bits').props.onClick(); });
    assertSort('bits', 'Bits', 'ascending');
    assert.deepEqual(rows(), ['catalogue-test/large-37B-4bit', 'catalogue-test/small-2B-8bit', 'catalogue-test/unknown']);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Grid view' }).props.onClick(); });
    assert.equal(renderer.root.findAllByProps({ className: 'discover-list-heading' }).length, 0);
    assert.equal(renderer.root.findAllByProps({ role: 'table', 'aria-label': 'Model catalogue' }).length, 0);
    assert.ok(renderer.root.findByProps({ className: 'catalogue-grid-sort', role: 'group' }).findAllByType('button').includes(sortButton('Bits')));
    assertSort('bits', 'Bits', 'ascending', false);
    await act(async () => { sortButton('Parameters').props.onClick(); });
    assertSort('parameters', 'Parameters', 'descending', false);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'List view' }).props.onClick(); });
    assertSort('parameters', 'Parameters', 'descending');
    assert.deepEqual(rows(), ['catalogue-test/large-37B-4bit', 'catalogue-test/small-2B-8bit', 'catalogue-test/unknown']);
    await act(async () => { sortButton('Bits').props.onClick(); });
    await act(async () => { sortButton('Bits').props.onClick(); });
    assert.deepEqual(JSON.parse(bridge.storage.get('studio.discover.preferences')!), { query, view: 'list', sort: 'bits', direction: 'asc' });
    await act(async () => { renderer.unmount(); renderer = TestRenderer.create(<SearchView snapshot={snapshot()} onAction={async () => ({})} />); await runTimers(); });
    assert.equal(bridge.calls.filter(call => call.method === 'search').length, 1);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'List view' }).props['aria-pressed'], true);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Search Hugging Face models' }).props.value, query);
    assertSort('bits', 'Bits', 'ascending');
    assert.deepEqual(rows(), ['catalogue-test/large-37B-4bit', 'catalogue-test/small-2B-8bit', 'catalogue-test/unknown']);
  } finally { await act(async () => renderer.unmount()); }
});

test('Discover retains a stale catalogue during background refresh and on a Hub failure', async () => {
  const { writeCatalogue } = await import('../src/catalogue');
  const query = 'ui-catalogue-offline';
  const pending = deferred<any>();
  const bridge = harness({ search: () => pending.promise });
  bridge.storage.set('studio.discover.preferences', JSON.stringify({ query, view: 'list', sort: 'downloads', direction: 'desc' }));
  writeCatalogue(query, [{ id: 'offline-test/cached-4bit', downloads: 7, createdAt: '2026-10-25T00:00:00Z', studio_compatibility: { status: 'compatible', reason: null }, studio_size: { weights_bytes: 1e9, weights_complete: true, download_bytes: 1.1e9, download_complete: true } }], Date.now() - 600_000);
  const tasks: Array<() => void> = [];
  window.setTimeout = ((callback: () => void) => { tasks.push(callback); return tasks.length; }) as typeof window.setTimeout;
  window.clearTimeout = (() => {}) as typeof window.clearTimeout;
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<SearchView snapshot={snapshot()} onAction={async () => ({})} />); await flush(); });
  try {
    assert.equal(renderer.root.findAllByProps({ className: 'discover-list-row' }).length, 1, 'cached rows render before any refresh response');
    await act(async () => { tasks.splice(0).forEach(run => run()); await flush(); });
    assert.equal(bridge.calls.filter(call => call.method === 'search').length, 1);
    assert.equal(renderer.root.findAllByProps({ className: 'discover-list-row' }).length, 1, 'the refresh keeps cached content visible');
    await act(async () => { pending.reject(new Error('Hub unavailable')); await flush(); });
    assert.equal(renderer.root.findAllByProps({ className: 'discover-list-row' }).length, 1);
    assert.match(renderedText(renderer.root.findByProps({ role: 'alert' })), /Hub unavailable/);
  } finally { await act(async () => renderer.unmount()); }
});

test('Discover preserves persisted immutable sizes when a background refresh returns bare off-screen models', async () => {
  const { readCatalogue, writeCatalogue } = await import('../src/catalogue');
  const query = 'ui-catalogue-refresh-sizes';
  const sha = 'cb'.repeat(20);
  const fresh = [
    { id: 'refresh-size-test/small-4bit', sha, downloads: 25, likes: 2, createdAt: '2026-10-25T00:00:00Z', studio_compatibility: { status: 'compatible', reason: null } },
    { id: 'refresh-size-test/large-8bit', sha, downloads: 50, likes: 3, createdAt: '2026-10-25T00:00:00Z', studio_compatibility: { status: 'compatible', reason: null } },
  ];
  const bridge = harness({ search: () => fresh });
  bridge.storage.set('studio.discover.preferences', JSON.stringify({ query, view: 'list', sort: 'disk', direction: 'desc' }));
  writeCatalogue(query, fresh.map((model, index) => ({
    ...model, downloads: 999, likes: 999,
    createdAt: '2026-10-25T00:00:00Z', studio_compatibility: { status: 'compatible', reason: null }, studio_size: { revision: sha, weights_bytes: (index + 1) * 1e9, weights_complete: true, download_bytes: (index + 1) * 1.1e9, download_complete: true },
  })), Date.now() - 600_000);
  const timers = new Map<number, () => void>();
  let nextTimer = 0;
  window.setTimeout = ((callback: () => void) => { timers.set(++nextTimer, callback); return nextTimer; }) as typeof window.setTimeout;
  window.clearTimeout = ((id: number) => { timers.delete(id); }) as typeof window.clearTimeout;
  const observer = Object.getOwnPropertyDescriptor(globalThis, 'IntersectionObserver');
  Object.defineProperty(globalThis, 'IntersectionObserver', {
    configurable: true,
    value: class { observe() {} disconnect() {} },
  });
  let renderer: TestRenderer.ReactTestRenderer;
  try {
    await act(async () => {
      renderer = TestRenderer.create(<SearchView snapshot={snapshot()} onAction={async () => ({})} />, { createNodeMock: element => element.type === 'article' ? {} : null });
      await flush();
    });
    const rows = () => renderer.root.findAllByProps({ className: 'discover-list-row' });
    assert.deepEqual(rows().map(row => row.props['aria-label']), ['refresh-size-test/large-8bit', 'refresh-size-test/small-4bit']);
    assert.match(renderedText(renderer.root.findByProps({ 'aria-label': 'Download size for refresh-size-test/large-8bit' })), /2\.20 GB/);
    await act(async () => { const pending = [...timers.values()]; timers.clear(); pending.forEach(run => run()); await flush(); });
    assert.equal(bridge.calls.filter(call => call.method === 'search').length, 1);
    assert.equal(bridge.calls.filter(call => call.method === 'model.info').length, 0, 'complete persisted sizes require no off-screen metadata requests');
    assert.deepEqual(rows().map(row => row.props['aria-label']), ['refresh-size-test/large-8bit', 'refresh-size-test/small-4bit']);
    assert.match(renderedText(renderer.root.findByProps({ 'aria-label': 'Download size for refresh-size-test/large-8bit' })), /2\.20 GB/);
    assert.ok(renderer.root.findByProps({ title: '50 downloads' }), 'downloads come from the refreshed search');
    assert.deepEqual(readCatalogue(query)?.results.map(model => model.likes), [2, 3], 'old metadata cannot replace fresh popularity');
    assert.ok(readCatalogue(query)!.updatedAt > Date.now() - 60_000, 'a successful search advances catalogue freshness');
  } finally {
    await act(async () => renderer?.unmount());
    if (observer) Object.defineProperty(globalThis, 'IntersectionObserver', observer);
    else delete (globalThis as any).IntersectionObserver;
  }
});

test('Discover keeps refreshed popularity when an older metadata request completes afterward', async () => {
  const { readCatalogue } = await import('../src/catalogue');
  const query = 'ui-catalogue-delayed-popularity';
  const sha = 'da'.repeat(20);
  const metadata = deferred<any>();
  let searches = 0;
  const stable = {
    id: 'delayed-popularity-test/stable-2B', sha, downloads: 20, likes: 2,
    createdAt: '2026-10-25T00:00:00Z', studio_compatibility: { status: 'compatible', reason: null }, studio_size: { revision: sha, weights_bytes: 2e9, weights_complete: true, download_bytes: 2.1e9, download_complete: true },
  };
  const bridge = harness({
    search: () => {
      searches += 1;
      return [{ id: 'delayed-popularity-test/refreshed', sha, downloads: searches === 1 ? 10 : 50, likes: searches === 1 ? 1 : 8, createdAt: '2026-10-25T00:00:00Z', studio_compatibility: { status: 'compatible', reason: null } }, stable];
    },
    'model.info': () => metadata.promise,
  });
  bridge.storage.set('studio.discover.preferences', JSON.stringify({ query, view: 'list', sort: 'downloads', direction: 'desc' }));
  const timers = new Map<number, () => void>();
  let nextTimer = 0;
  window.setTimeout = ((callback: () => void) => { timers.set(++nextTimer, callback); return nextTimer; }) as typeof window.setTimeout;
  window.clearTimeout = ((id: number) => { timers.delete(id); }) as typeof window.clearTimeout;
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => {
    renderer = TestRenderer.create(<SearchView snapshot={snapshot()} onAction={async () => ({})} />);
    await flush();
    const pending = [...timers.values()]; timers.clear(); pending.forEach(run => run());
    await flush();
  });
  try {
    const rows = () => renderer.root.findAllByProps({ className: 'discover-list-row' }).map(row => row.props['aria-label']);
    assert.equal(bridge.calls.filter(call => call.method === 'model.info').length, 1, 'the original metadata request is still pending');
    assert.deepEqual(rows(), ['delayed-popularity-test/stable-2B', 'delayed-popularity-test/refreshed']);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Refresh catalogue' }).props.onClick(); await flush(); });
    assert.equal(searches, 2);
    assert.deepEqual(rows(), ['delayed-popularity-test/refreshed', 'delayed-popularity-test/stable-2B']);
    await act(async () => {
      metadata.resolve({
        id: 'delayed-popularity-test/refreshed', sha, downloads: 10, likes: 1,
        createdAt: '2026-10-25T00:00:00Z', studio_compatibility: { status: 'compatible', reason: null }, studio_size: { revision: sha, weights_bytes: 1e9, weights_complete: true, download_bytes: 1.1e9, download_complete: true },
      });
      await flush();
    });
    assert.deepEqual(rows(), ['delayed-popularity-test/refreshed', 'delayed-popularity-test/stable-2B'], 'late file metadata cannot reorder fresh download counts');
    assert.ok(renderer.root.findByProps({ title: '50 downloads' }));
    assert.match(renderedText(renderer.root.findByProps({ 'aria-label': 'Download size for delayed-popularity-test/refreshed' })), /1\.10 GB/, 'late file metadata is still applied');
    const cached = readCatalogue(query)!.results.find(model => model.id === 'delayed-popularity-test/refreshed');
    assert.equal(cached?.downloads, 50);
    assert.equal(cached?.likes, 8);
  } finally { await act(async () => renderer.unmount()); }
});

test('sidebar deletion confirms the named chat without selecting it or losing another composer draft', async () => {
  const bridge = harness({ 'conversations.remove': params => { bridge.data.conversations = bridge.data.conversations.filter(item => item.id !== params.id); return { removed: params.id }; } });
  bridge.data.folders.push({ id: 'delete-folder', name: 'Projects' });
  const first = { id: 'delete-a', title: 'Keep active chat', model: 'test', messages: [{ role: 'user', content: 'Keep these messages' }] };
  const second = { id: 'delete-b', title: 'Remove other chat', folder_id: 'delete-folder', model: 'test', messages: [{ role: 'assistant', content: 'Other answer' }] };
  bridge.data.conversations.push(first, second);
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { conversationItem(renderer.root, first.title).props.onClick({}); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: 'Keep my unsent draft' } }); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': `Delete conversation ${second.title}` }).props.onClick(); });
    const dialog = renderer.root.findByProps({ role: 'dialog' });
    assert.match(renderedText(dialog), /Remove other chat/);
    assert.match(renderedText(dialog), /cannot be undone/);
    assert.equal(bridge.calls.filter(call => call.method === 'conversations.remove').length, 0);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Message' }).props.value, 'Keep my unsent draft');
    await act(async () => { dialog.findAllByType('button').find(button => button.props.children === 'Cancel')!.props.onClick(); });
    assert.equal(renderer.root.findAllByProps({ role: 'dialog' }).length, 0);
    assert.equal(bridge.data.conversations.length, 2);
    await act(async () => { renderer.root.findByProps({ 'aria-label': `Delete conversation ${second.title}` }).props.onClick(); });
    await act(async () => { await renderer.root.findByProps({ 'aria-label': 'Confirm delete conversation' }).props.onClick(); await flush(); });
    assert.deepEqual(bridge.calls.filter(call => call.method === 'conversations.remove').map(call => call.params.id), [second.id]);
    assert.deepEqual(bridge.data.conversations, [first]);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Message' }).props.value, 'Keep my unsent draft');
    assert.match(conversationItem(renderer.root, first.title).props.className, /selected/);
    assert.equal(renderer.root.findAllByProps({ 'aria-label': `Delete conversation ${second.title}` }).length, 0);
  } finally { await act(async () => renderer.unmount()); }
});

test('composer deletion also confirms, handles Escape and failures, and sends only one pending request', async () => {
  let removal = deferred<any>();
  const bridge = harness({ 'conversations.remove': async params => { await removal.promise; bridge.data.conversations = bridge.data.conversations.filter(item => item.id !== params.id); return { removed: params.id }; } });
  const saved = { id: 'delete-current', title: 'Current conversation', model: 'test', messages: [{ role: 'user', content: 'A saved message' }] };
  bridge.data.conversations.push(saved);
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { conversationItem(renderer.root, saved.title).props.onClick({}); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Delete conversation' }).props.onClick(); });
    assert.equal(bridge.calls.some(call => call.method === 'conversations.remove'), false);
    await act(async () => { renderer.root.findByProps({ role: 'dialog' }).props.onKeyDown({ key: 'Escape', stopPropagation() {} }); });
    assert.equal(renderer.root.findAllByProps({ role: 'dialog' }).length, 0);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Delete conversation' }).props.onClick(); });
    await act(async () => { const confirm = renderer.root.findByProps({ 'aria-label': 'Confirm delete conversation' }); confirm.props.onClick(); confirm.props.onClick(); await flush(); });
    assert.equal(bridge.calls.filter(call => call.method === 'conversations.remove').length, 1);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Confirm delete conversation' }).props.disabled, true);
    await act(async () => { removal.reject(new Error('Could not save deletion')); await flush(); });
    assert.match(renderedText(renderer.root.findByProps({ role: 'dialog' })), /Could not save deletion/);
    assert.deepEqual(bridge.data.conversations, [saved]);
    removal = deferred<any>();
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Confirm delete conversation' }).props.onClick(); await flush(); removal.resolve({}); await flush(); });
    assert.equal(bridge.calls.filter(call => call.method === 'conversations.remove').length, 2);
    assert.equal(bridge.data.conversations.length, 0);
    assert.equal(renderer.root.findAllByProps({ role: 'dialog' }).length, 0);
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Delete conversation' }).length, 0);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Message' }).props.value, '');
  } finally { removal.resolve({}); await act(async () => renderer.unmount()); }
});

test('a reply starting while deletion is awaiting confirmation blocks deletion from either location', async () => {
  harness();
  const data = snapshot();
  const saved = { id: 'delete-stream', title: 'Streaming conversation', model: 'test', messages: [{ role: 'user', content: 'Question' }] };
  data.conversations.push(saved);
  const calls: string[] = [];
  const props = { active: true, snapshot: data, streams: {} as Record<string, string>, errors: {}, onSend: async () => {}, onSave: async () => {}, onAction: async (method: string) => { calls.push(method); return {}; }, onServer: () => {} };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ChatView {...props} />); });
  try {
    await act(async () => { conversationItem(renderer.root, saved.title).props.onClick({}); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': `Delete conversation ${saved.title}` }).props.onClick(); });
    await act(async () => { renderer.update(<ChatView {...props} streams={{ [saved.id]: 'starting:request' }} />); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Delete conversation' }).props.disabled, true);
    assert.equal(renderer.root.findByProps({ 'aria-label': `Delete conversation ${saved.title}` }).props.disabled, true);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Confirm delete conversation' }).props.disabled, true);
    assert.match(renderedText(renderer.root.findByProps({ role: 'dialog' })), /Stop the running reply/);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Confirm delete conversation' }).props.onClick(); });
    assert.equal(calls.includes('conversations.remove'), false);
    await act(async () => { renderer.update(<ChatView {...props} />); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Confirm delete conversation' }).props.disabled, false);
  } finally { await act(async () => renderer.unmount()); }
});

test('a snapshot already pending during deletion cannot restore the row or let a stale Send handler recreate it', async () => {
  const stale = deferred<any>();
  let holdSnapshot = false;
  const bridge = harness({ snapshot: () => holdSnapshot ? stale.promise : structuredClone(bridge.data), 'conversations.remove': params => { bridge.data.conversations = bridge.data.conversations.filter(item => item.id !== params.id); return { removed: params.id }; } });
  const saved = { id: 'delete-stale', title: 'Do not resurrect', model: 'test', messages: [{ role: 'user', content: 'Saved content' }] };
  bridge.data.conversations.push(saved);
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { conversationItem(renderer.root, saved.title).props.onClick({}); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: 'Queued message' } }); });
    const staleSend = renderer.root.findByProps({ 'aria-label': 'Send message' }).props.onClick;
    const oldSnapshot = structuredClone(bridge.data);
    holdSnapshot = true;
    await act(async () => { bridge.emit('conversations.changed', {}); await flush(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': `Delete conversation ${saved.title}` }).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Confirm delete conversation' }).props.onClick(); await flush(); });
    assert.equal(renderer.root.findAllByProps({ 'aria-label': `Delete conversation ${saved.title}` }).length, 0, 'the deleted row disappears before the pending snapshot returns');
    await act(async () => { staleSend(); await flush(); });
    assert.equal(bridge.calls.some(call => call.method === 'conversations.save'), false, 'a queued Send cannot recreate the deleted ID');
    assert.equal(bridge.calls.some(call => call.method === 'chat.start'), false);
    await act(async () => { stale.resolve(oldSnapshot); await flush(); });
    assert.equal(renderer.root.findAllByProps({ 'aria-label': `Delete conversation ${saved.title}` }).length, 0, 'the pre-delete snapshot cannot resurrect the row');
    assert.equal(bridge.data.conversations.length, 0);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Message' }).props.value, '');
  } finally { stale.resolve(structuredClone(bridge.data)); await act(async () => renderer.unmount()); }
});

test('Server filters checked models in alphabetical order and keeps its filter and inspected logs across navigation', async () => {
  const bridge = harness();
  bridge.data.models.push({ ...model, id: 'm2', name: 'Beta model' }, { ...model, id: 'm3', name: 'Gamma model' });
  bridge.data.profiles = [{ ...profile, id: 'gamma', alias: 'gamma', model_id: 'm3', selected: true }, { ...profile, id: 'beta', alias: 'beta', model_id: 'm2', selected: false }, { ...profile, id: 'alpha', alias: 'alpha', selected: true }];
  bridge.data.servers = [{ profile_id: 'beta', model_id: 'm2', alias: 'beta', state: 'ready', active_requests: 0 }];
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  const rows = () => renderer.root.findAllByType('div').filter(node => String(node.props.className).split(' ').includes('server-row'));
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Model server' }).props.onClick(); await flush(); });
    assert.deepEqual(rows().map(row => row.props['aria-label']), ['Server alpha', 'Server beta', 'Server gamma']);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Logs for beta' }).props.onClick(); await flush(); });
    const inspected = renderer.root.findByProps({ role: 'group', 'aria-label': 'Server beta' });
    assert.ok(inspected.props.className.split(' ').includes('focused'));
    assert.equal(inspected.props.className.split(' ').includes('selected'), false, 'inspecting a model does not mark it for global start');
    assert.ok(renderer.root.findByProps({ role: 'group', 'aria-label': 'Server alpha' }).props.className.split(' ').includes('selected'));
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Show selected models only' }).props.onChange({ target: { checked: true } }); });
    assert.deepEqual(rows().map(row => row.props['aria-label']), ['Server alpha', 'Server gamma']);
    assert.match(renderedText(renderer.root.findByProps({ className: 'subtle server-visible-count' })), /2\s+of\s+3\s+models/);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Server logs profile' }).props.value, 'beta');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Stop server' }).props.disabled, false, 'global stop still covers a running worker hidden by the filter');
    assert.equal(bridge.storage.get('studio.server.selectedOnly'), 'true');
    await act(async () => { bridge.emit('telemetry', { system: { cpu_percent: 32 }, workers: [], managed_totals: {} }); });
    assert.deepEqual(rows().map(row => row.props['aria-label']), ['Server alpha', 'Server gamma']);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Chat' }).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Model server' }).props.onClick(); await flush(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Show selected models only' }).props.checked, true);
    assert.deepEqual(rows().map(row => row.props['aria-label']), ['Server alpha', 'Server gamma']);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Server logs profile' }).props.value, 'beta');
    assert.equal(bridge.calls.some(call => ['profiles.save', 'server.start', 'server.stop', 'config.set'].includes(call.method)), false, 'filtering does not change serving selection, lifecycle or configuration');
  } finally { await act(async () => renderer.unmount()); }
});

test('unchecking the last filtered Server model shows a recoverable empty state without changing other profiles', async () => {
  const bridge = harness({ 'profiles.save': params => { bridge.data.profiles = bridge.data.profiles.map(item => item.id === params.profile.id ? structuredClone(params.profile) : item); return params.profile; } });
  bridge.data.profiles[0] = { ...bridge.data.profiles[0], alias: 'alpha' };
  bridge.data.profiles.push({ ...profile, id: 'beta', alias: 'beta', selected: false });
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Model server' }).props.onClick(); await flush(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Show selected models only' }).props.onChange({ target: { checked: true } }); });
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Select beta' }).length, 0);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Select alpha' }).props.onChange({ target: { checked: false } }); await flush(); });
    assert.match(renderedText(renderer.root.findByProps({ className: 'server-view' })), /No selected models/);
    assert.match(renderedText(renderer.root.findByProps({ className: 'subtle server-visible-count' })), /0\s+of\s+2\s+models/);
    assert.equal(bridge.calls.filter(call => call.method === 'profiles.save').length, 1);
    assert.equal(bridge.data.profiles.every(item => item.selected === false), true);
    await act(async () => { renderer.root.findAllByType('button').find(button => renderedText(button) === 'Show all models')!.props.onClick(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Show selected models only' }).props.checked, false);
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Select alpha' }).length, 1);
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Select beta' }).length, 1);
    assert.equal(bridge.storage.get('studio.server.selectedOnly'), 'false');
    assert.equal(bridge.calls.some(call => ['server.start', 'server.stop'].includes(call.method)), false);
  } finally { await act(async () => renderer.unmount()); }
});

test('Discover bit headers sort the precision shown for dtype tags and top-level quantization', async () => {
  const { writeCatalogue, writeDiscoverPreferences } = await import('../src/catalogue');
  harness();
  const query = 'ui-discover-precision-labels';
  writeDiscoverPreferences({ query, view: 'list', sort: 'bits', direction: 'desc' });
  const size = { weights_bytes: 1e9, weights_complete: true, download_bytes: 1.1e9, download_complete: true };
  writeCatalogue(query, [
    { id: 'precision-test/unknown', createdAt: '2026-10-25T00:00:00Z', studio_compatibility: { status: 'compatible', reason: null }, studio_size: size },
    { id: 'precision-test/dtype-only', tags: ['bfloat16'], createdAt: '2026-10-25T00:00:00Z', studio_compatibility: { status: 'compatible', reason: null }, studio_size: size },
    { id: 'precision-test/config-only', quantization: 'int8', createdAt: '2026-10-25T00:00:00Z', studio_compatibility: { status: 'compatible', reason: null }, studio_size: size },
  ]);
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<SearchView snapshot={snapshot()} onAction={async () => ({})} />); await flush(); });
  try {
    const rows = () => renderer.root.findAllByProps({ className: 'discover-list-row' });
    const values = () => rows().map(row => renderedText(row.findByProps({ className: 'catalogue-quantization' })));
    assert.deepEqual(values(), ['16-bit', '8-bit', 'Unknown']);
    await act(async () => { renderer.root.findByProps({ 'data-sort': 'bits' }).props.onClick(); });
    assert.deepEqual(values(), ['8-bit', '16-bit', 'Unknown']);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Grid view' }).props.onClick(); });
    assert.match(renderedText(renderer.root.findByProps({ className: 'discover-grid' })), /16-bit/);
  } finally { await act(async () => renderer.unmount()); }
});

test('folder removal confirms its name and conversation count, preserving messages and an unsent draft', async () => {
  const bridge = harness({ 'chat.start': () => ({ request_id: 'folder-draft-reply' }) });
  bridge.data.folders.push({ id: 'remove-project', name: 'Projects' }, { id: 'keep-folder', name: 'Keep folder' });
  const first = { id: 'keep-one', title: 'First saved chat', folder_id: 'remove-project', model: 'test', messages: [{ role: 'user', content: 'Keep the first message' }] };
  const second = { id: 'keep-two', title: 'Second saved chat', folder_id: 'remove-project', model: 'test', messages: [{ role: 'assistant', content: 'Keep this answer' }] };
  const other = { id: 'keep-other', title: 'Other folder chat', folder_id: 'keep-folder', model: 'test', messages: [] };
  bridge.data.conversations.push(first, second, other);
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'New conversation in Projects' }).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: 'Keep my folder draft' } }); });
    const selectedModel = renderer.root.findByProps({ 'aria-label': 'Chat model' }).props.value;
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Remove folder Projects' }).props.onClick(); });
    const dialog = () => renderer.root.findByProps({ role: 'dialog', 'aria-label': 'Remove folder?' });
    assert.match(renderedText(dialog()), /Projects/);
    assert.match(renderedText(dialog()), /2 conversations will be moved to No folder/);
    assert.match(renderedText(dialog()), /All messages will be kept/);
    assert.match(renderedText(dialog()), /unsent draft will be kept/);
    assert.equal(bridge.calls.some(call => call.method === 'folders.remove'), false);
    await act(async () => { dialog().findAllByType('button').find(button => button.props.children === 'Cancel')!.props.onClick(); });
    assert.equal(bridge.data.folders.length, 2);
    assert.equal(bridge.data.conversations.filter(item => item.folder_id === 'remove-project').length, 2);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Message' }).props.value, 'Keep my folder draft');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Remove folder Projects' }).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Confirm remove folder' }).props.onClick(); await flush(); });
    assert.deepEqual(bridge.calls.filter(call => call.method === 'folders.remove').map(call => call.params.id), ['remove-project']);
    assert.equal(bridge.calls.some(call => call.method === 'conversations.remove'), false);
    assert.deepEqual(bridge.data.folders, [{ id: 'keep-folder', name: 'Keep folder' }]);
    assert.deepEqual(bridge.data.conversations, [{ ...first, folder_id: null }, { ...second, folder_id: null }, other]);
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Conversation folder' }).length, 0, 'the unsaved draft is now unfiled');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Message' }).props.value, 'Keep my folder draft');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Chat model' }).props.value, selectedModel);
    const unfiled = renderer.root.findByProps({ role: 'group', 'aria-label': 'Conversations without a folder' });
    assert.ok(conversationItem(unfiled, first.title));
    assert.ok(conversationItem(unfiled, second.title));
    assert.equal(renderer.root.findAllByProps({ role: 'dialog' }).length, 0);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Send message' }).props.disabled, false, renderer.root.findByProps({ 'aria-label': 'Send message' }).props.title);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Send message' }).props.onClick(); await flush(); });
    const savedDraft = bridge.calls.filter(call => call.method === 'conversations.save').at(-1)!.params.conversation;
    assert.equal(savedDraft.folder_id, null);
    assert.equal(savedDraft.messages.at(-1).content, 'Keep my folder draft');
    assert.equal(savedDraft.model, selectedModel);
  } finally { await act(async () => renderer.unmount()); }
});

test('empty folder removal supports Escape, duplicate-click protection, and retry after failure', async () => {
  let removal = deferred<any>();
  const bridge = harness({ 'folders.remove': async params => { await removal.promise; bridge.data.folders = bridge.data.folders.filter(item => item.id !== params.id); return { removed: params.id }; } });
  bridge.data.folders.push({ id: 'empty-remove', name: 'Empty project' });
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Remove folder Empty project' }).props.onClick(); });
    assert.match(renderedText(renderer.root.findByProps({ role: 'dialog' })), /no saved conversations/);
    await act(async () => { renderer.root.findByProps({ role: 'dialog' }).props.onKeyDown({ key: 'Escape', stopPropagation() {} }); });
    assert.equal(renderer.root.findAllByProps({ role: 'dialog' }).length, 0);
    assert.equal(bridge.calls.some(call => call.method === 'folders.remove'), false);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Remove folder Empty project' }).props.onClick(); });
    await act(async () => { const confirm = renderer.root.findByProps({ 'aria-label': 'Confirm remove folder' }); confirm.props.onClick(); confirm.props.onClick(); await flush(); });
    assert.equal(bridge.calls.filter(call => call.method === 'folders.remove').length, 1);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Confirm remove folder' }).props.disabled, true);
    await act(async () => { removal.reject(new Error('Could not remove folder')); await flush(); });
    assert.match(renderedText(renderer.root.findByProps({ role: 'dialog' })), /Could not remove folder/);
    assert.equal(bridge.data.folders.length, 1);
    removal = deferred<any>();
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Confirm remove folder' }).props.onClick(); await flush(); removal.resolve({}); await flush(); });
    assert.equal(bridge.data.folders.length, 0);
    assert.equal(renderer.root.findAllByProps({ role: 'dialog' }).length, 0);
    assert.equal(bridge.calls.filter(call => call.method === 'folders.remove').length, 2);
  } finally { removal.resolve({}); await act(async () => renderer.unmount()); }
});

test('a removed folder cannot return through an older snapshot, move overlay, or queued Send', async () => {
  const stale = deferred<any>();
  let holdSnapshot = false;
  const bridge = harness({ snapshot: () => holdSnapshot ? stale.promise : structuredClone(bridge.data), 'chat.start': () => ({ request_id: 'folder-queued-reply' }) });
  bridge.data.folders.push({ id: 'inbox', name: 'Inbox' }, { id: 'removed-destination', name: 'Projects' });
  const saved = { id: 'folder-stale-chat', title: 'Preserve moved chat', folder_id: 'inbox', model: 'test', messages: [{ role: 'user', content: 'Saved before the move' }] };
  bridge.data.conversations.push(saved);
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { conversationItem(renderer.root, saved.title).props.onClick({}); });
    const beforeMove = structuredClone(bridge.data);
    holdSnapshot = true;
    await act(async () => { bridge.emit('folders.changed', {}); await flush(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Conversation folder' }).props.onChange({ target: { value: 'removed-destination' } }); await flush(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Conversation folder' }).props.value, 'removed-destination');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: 'Queued after the move' } }); });
    const queuedSend = renderer.root.findByProps({ 'aria-label': 'Send message' }).props.onClick;
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Remove folder Projects' }).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Confirm remove folder' }).props.onClick(); await flush(); });
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Remove folder Projects' }).length, 0);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Conversation folder' }).props.value, '');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Message' }).props.value, 'Queued after the move');
    await act(async () => { queuedSend(); await flush(); });
    const stored = bridge.calls.filter(call => call.method === 'conversations.save').at(-1)!;
    assert.equal(stored.params.conversation.folder_id, null);
    assert.deepEqual(stored.params.conversation.messages.map((message: any) => message.content), ['Saved before the move', 'Queued after the move']);
    holdSnapshot = false;
    await act(async () => { stale.resolve(beforeMove); await flush(); });
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Remove folder Projects' }).length, 0, 'the stale folder cannot return');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Conversation folder' }).props.value, '', 'normalization runs after the old move overlay');
    assert.ok(conversationItem(renderer.root.findByProps({ 'aria-label': 'Conversations without a folder' }), saved.title));
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Chat model' }).props.value, 'test');
  } finally { stale.resolve(structuredClone(bridge.data)); await act(async () => renderer.unmount()); }
});

test('removing a folder during a reply preserves the live answer, model and streaming controls', async () => {
  const bridge = harness({ 'chat.start': () => ({ request_id: 'folder-live-reply' }) });
  bridge.data.folders.push({ id: 'live-folder', name: 'Writing' });
  bridge.data.conversations.push({ id: 'folder-live-chat', title: 'Live folder chat', folder_id: 'live-folder', model: 'test', messages: [{ role: 'user', content: 'Earlier question' }] });
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { conversationItem(renderer.root, 'Live folder chat').props.onClick({}); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: 'Continue this answer' } }); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Send message' }).props.onClick(); await flush(); });
    await act(async () => { bridge.emit('chat.delta', { conversation_id: 'folder-live-chat', request_id: 'folder-live-reply', delta: 'Partial answer kept' }); await flush(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Remove folder Writing' }).props.disabled, false);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Remove folder Writing' }).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Confirm remove folder' }).props.onClick(); await flush(); });
    assert.ok(renderer.root.findAllByType(Markdown).some(node => node.props.children === 'Partial answer kept'));
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Conversation folder' }).props.value, '');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Chat model' }).props.value, 'test');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Stop generating' }).props.disabled, false);
    assert.equal(bridge.calls.some(call => ['chat.cancel', 'conversations.remove'].includes(call.method)), false);
    const final = { role: 'assistant', content: 'Partial answer kept and completed' };
    bridge.data.conversations[0].messages.push(final);
    await act(async () => { bridge.emit('chat.done', { conversation_id: 'folder-live-chat', request_id: 'folder-live-reply', message: final }); await flush(); });
    assert.equal(bridge.data.conversations[0].folder_id, null);
    assert.ok(renderer.root.findAllByType(Markdown).some(node => node.props.children === final.content));
  } finally { await act(async () => renderer.unmount()); }
});

test('Discover shows published 2B, 4B, 9B and 37B sizes alongside download size in list, grid and details', async () => {
  const { writeCatalogue, writeDiscoverPreferences } = await import('../src/catalogue');
  const query = 'ui-published-parameter-sizes';
  const sha = 'ef'.repeat(20);
  const models = [2, 4, 9, 37].map(count => ({
    id: `parameter-ui/Model-${count}B${count === 37 ? '-A3B' : ''}-4bit`, sha,
    tags: ['mlx', '4bit'], safetensors: { total: 42_000_000 },
    createdAt: '2026-10-25T00:00:00Z', studio_compatibility: { status: 'compatible', reason: null }, studio_size: { revision: sha, weights_bytes: 123, weights_complete: true, download_bytes: 9_870_000_000, download_complete: true },
  }));
  const unknown = { id: 'parameter-ui/unknown', sha, tags: ['mlx', '4bit'], safetensors: { total: 8_000_000_000 }, createdAt: '2026-10-25T00:00:00Z', studio_compatibility: { status: 'compatible', reason: null }, studio_size: { revision: sha, weights_bytes: 2e9, weights_complete: true, download_bytes: 2.1e9, download_complete: true } };
  harness({ 'model.info': params => models.find(model => model.id === params.repo_id) ?? unknown });
  writeDiscoverPreferences({ query, view: 'list', sort: 'parameters', direction: 'desc' });
  writeCatalogue(query, [...models, unknown]);
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<SearchView snapshot={snapshot()} onAction={async () => ({})} />); await flush(); });
  try {
    const rows = () => renderer.root.findAllByProps({ className: 'discover-list-row' });
    const labels = () => rows().map(row => renderedText(row.findByProps({ 'aria-label': `Parameters for ${row.props['aria-label']}` })));
    assert.deepEqual(labels(), ['37B', '9B', '4B', '2B', 'Unknown']);
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Sort by Weights' }).length, 0);
    assert.ok(renderer.root.findByProps({ 'aria-label': 'Sort by Download size' }));
    assert.match(renderedText(rows()[0].findByProps({ 'aria-label': `Download size for ${models[3].id}` })), /9\.87 GB/);
    await act(async () => { renderer.root.findByProps({ 'data-sort': 'parameters' }).props.onClick(); });
    assert.deepEqual(labels(), ['2B', '4B', '9B', '37B', 'Unknown']);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Grid view' }).props.onClick(); await flush(); });
    const grid = renderer.root.findByProps({ className: 'discover-grid' });
    assert.doesNotMatch(renderedText(grid), /Model weights|Weights on disk/);
    for (const model of models) {
      const expected = model.id.match(/-(\d+)B/)![1];
      assert.match(renderedText(grid.findByProps({ 'aria-label': `Parameters for ${model.id}` })), new RegExp(`${expected}B`));
      assert.match(renderedText(grid.findByProps({ 'aria-label': `Download size for ${model.id}` })), /9\.87 GB/);
    }
    const card = grid.findAllByProps({ className: 'discover-card' }).find(card => renderedText(card).includes('37B-A3B'))!;
    await act(async () => { card.findByProps({ className: 'discover-card-title' }).props.onClick(); await flush(); });
    const details = renderer.root.findByProps({ role: 'dialog', 'aria-label': 'Download model' });
    const values = details.findByProps({ className: 'download-details-grid discover-download-sizes' });
    assert.match(renderedText(values), /37B/);
    assert.match(renderedText(values), /9\.87 GB/);
    assert.doesNotMatch(renderedText(values), /Model weights|42M/);
  } finally { await act(async () => renderer.unmount()); }
});


test('Start selected only starts profiles that are stopped or failed and blocks duplicate global and row starts', async () => {
  harness();
  const data = snapshot();
  const calls: any[] = [];
  const pending = deferred<any>();
  const onAction = async (method: string, params: any) => { calls.push({ method, params }); return method === 'server.start' ? pending.promise : {}; };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={data} onAction={onAction} onLibrary={() => {}} />); await flush(); });
  try {
    const start = () => renderer.root.findByProps({ 'aria-label': 'Start selected' });
    for (const state of ['running', 'ready', 'starting', 'loading', 'draining', 'stopping']) {
      data.servers[0].state = state;
      await act(async () => { renderer.update(<ServerView snapshot={structuredClone(data)} onAction={onAction} onLibrary={() => {}} />); });
      assert.equal(start().props.disabled, true, state);
      await act(async () => { start().props.onClick(); await flush(); });
    }
    assert.deepEqual(calls, [], 'even stale click handlers cannot restart occupied profiles');
    data.servers[0].state = 'ready';
    data.models.push({ ...model, id: 'm2' });
    data.profiles.push({ ...profile, id: 'p2', model_id: 'm2', alias: 'new-model', selected: false });
    data.servers.push({ profile_id: 'p2', model_id: 'm2', alias: 'new-model', state: 'failed', active_requests: 0 });
    await act(async () => { renderer.update(<ServerView snapshot={structuredClone(data)} onAction={onAction} onLibrary={() => {}} />); });
    assert.equal(start().props.disabled, true, 'an unselected failed model does not enable global start');
    data.profiles[1].selected = true;
    await act(async () => { renderer.update(<ServerView snapshot={structuredClone(data)} onAction={onAction} onLibrary={() => {}} />); });
    assert.equal(start().props.disabled, false, 'selecting the failed model enables start');
    const staleGlobal = start().props.onClick;
    const staleRow = renderer.root.findByProps({ className: 'start-server' }).props.onClick;
    await act(async () => { staleGlobal(); staleGlobal(); staleRow(); await flush(); });
    assert.deepEqual(calls, [{ method: 'server.start', params: { all: true } }]);
    assert.equal(start().props.disabled, true);
    assert.equal(renderer.root.findByProps({ className: 'start-server' }).props.disabled, true, 'a global start owns its row start');
    await act(async () => { pending.resolve({}); await flush(); });
    data.servers[1].state = 'loading';
    await act(async () => { renderer.update(<ServerView snapshot={structuredClone(data)} onAction={onAction} onLibrary={() => {}} />); });
    assert.equal(start().props.disabled, true);
  } finally { pending.resolve({}); await act(async () => renderer.unmount()); }
});

test('a pending individual start blocks Start selected and another stopped row until it completes', async () => {
  harness();
  const data = snapshot(); data.servers[0].state = 'stopped';
  data.models.push({ ...model, id: 'm2' });
  data.profiles.push({ ...profile, id: 'p2', model_id: 'm2', alias: 'second' });
  data.servers.push({ profile_id: 'p2', model_id: 'm2', alias: 'second', state: 'stopped', active_requests: 0 });
  const pending = deferred<any>();
  const calls: any[] = [];
  const onAction = async (method: string, params: any) => { calls.push({ method, params }); return pending.promise; };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={data} onAction={onAction} onLibrary={() => {}} />); await flush(); });
  try {
    const rows = renderer.root.findAllByProps({ className: 'start-server' });
    const staleGlobal = renderer.root.findByProps({ 'aria-label': 'Start selected' }).props.onClick;
    await act(async () => { rows[0].props.onClick(); staleGlobal(); rows[1].props.onClick(); await flush(); });
    assert.equal(calls.length, 1);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Start selected' }).props.disabled, true);
    assert.ok(renderer.root.findAllByProps({ className: 'start-server' }).every(row => row.props.disabled));
  } finally { pending.resolve({}); await act(async () => renderer.unmount()); }
});

test('Studio status distinguishes stopped inference, an empty API, model loading and serving', async () => {
  const bridge = harness();
  bridge.data.api = { running: false, enabled: false };
  bridge.data.servers[0].state = 'stopped';
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    const status = () => renderer.root.findByProps({ 'aria-label': 'Studio status' });
    assert.match(renderedText(status()), /Studio connected.*inference stopped/);
    assert.equal(status().props['data-state'], 'idle');
    await act(async () => { bridge.emit('api.changed', { running: true, enabled: true }); });
    assert.match(renderedText(status()), /Studio connected.*no models running/);
    assert.equal(status().props['data-state'], 'idle');
    await act(async () => { bridge.emit('server.state', { ...bridge.data.servers[0], state: 'loading' }); });
    assert.match(renderedText(status()), /models starting/);
    assert.equal(status().props['data-state'], 'idle');
    await act(async () => { bridge.emit('server.state', { ...bridge.data.servers[0], state: 'ready' }); });
    assert.match(renderedText(status()), /1 model serving/);
    assert.equal(status().props['data-state'], 'online');
    await act(async () => { bridge.emit('api.changed', { running: false, enabled: true, error: 'Port is busy' }); });
    assert.match(renderedText(status()), /inference unavailable/);
    assert.equal(status().props['data-state'], 'offline');
    assert.doesNotMatch(renderedText(renderer.root.findByProps({ className: 'status-bar' })), /1 model serving/);
  } finally { await act(async () => renderer.unmount()); }
});

test('event disconnection immediately clears green and a pre-disconnect snapshot cannot restore it', async () => {
  const pending = deferred<any>();
  let delayed = false;
  const bridge = harness({ snapshot: () => delayed ? pending.promise : structuredClone(bridge.data) });
  const timers = controlledIntervals();
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    const status = () => renderer.root.findByProps({ 'aria-label': 'Studio status' });
    assert.equal(status().props['data-state'], 'online');
    delayed = true;
    await act(async () => { timers.tick(); await flush(); });
    const beforeDisconnect = structuredClone(bridge.data);
    await act(async () => { bridge.emit('service.disconnected', { error: 'Event connection closed' }); });
    assert.match(renderedText(status()), /reconnecting/);
    assert.notEqual(status().props['data-state'], 'online');
    assert.match(renderedText(renderer.root.findByProps({ className: 'connection-banner' })), /Event connection closed/);
    await act(async () => { pending.resolve(beforeDisconnect); await flush(); });
    assert.notEqual(status().props['data-state'], 'online', 'the stale read is ignored');
    assert.doesNotMatch(renderedText(renderer.root.findByProps({ className: 'status-bar' })), /model serving/);
    delayed = false;
    await act(async () => { timers.tick(); await flush(); });
    assert.notEqual(status().props['data-state'], 'online', 'a healthy RPC does not repair a disconnected event stream');
    await act(async () => { bridge.emit('connected', {}); await flush(); });
    assert.equal(status().props['data-state'], 'online');
    assert.equal(renderer.root.findAllByProps({ className: 'connection-banner' }).length, 0);
  } finally { pending.resolve(bridge.data); await act(async () => renderer.unmount()); }
});

test('a failed event subscription retries without allowing snapshot success to hide the failure', async () => {
  let attempts = 0;
  const bridge = harness({ studio_events: () => { if (++attempts === 1) throw new Error('Cannot subscribe to Studio events'); bridge.emit('connected', {}); } });
  const timers = controlledIntervals();
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    const status = () => renderer.root.findByProps({ 'aria-label': 'Studio status' });
    assert.match(renderedText(status()), /Studio updates reconnecting/);
    assert.notEqual(status().props['data-state'], 'online');
    assert.match(renderedText(renderer.root.findByProps({ className: 'connection-banner' })), /Cannot subscribe/);
    await act(async () => { timers.tick(); await flush(); });
    assert.equal(attempts, 2);
    assert.equal(status().props['data-state'], 'online');
    assert.equal(renderer.root.findAllByProps({ className: 'connection-banner' }).length, 0);
  } finally { await act(async () => renderer.unmount()); }
});


test('a failed service poll marks the status offline until a fresh snapshot succeeds', async () => {
  let unavailable = false;
  const bridge = harness({ snapshot: () => { if (unavailable) throw new Error('Studio service is unreachable'); return structuredClone(bridge.data); } });
  const timers = controlledIntervals();
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    const status = () => renderer.root.findByProps({ 'aria-label': 'Studio status' });
    assert.equal(status().props['data-state'], 'online');
    unavailable = true;
    await act(async () => { timers.tick(); await flush(); });
    assert.equal(status().props['data-state'], 'offline');
    assert.match(renderedText(status()), /Studio service offline/);
    assert.match(renderedText(renderer.root.findByProps({ className: 'connection-banner' })), /Studio service is unreachable/);
    assert.doesNotMatch(renderedText(renderer.root.findByProps({ className: 'status-bar' })), /model serving/);
    unavailable = false;
    await act(async () => { timers.tick(); await flush(); });
    assert.equal(status().props['data-state'], 'online');
    assert.equal(renderer.root.findAllByProps({ className: 'connection-banner' }).length, 0);
  } finally { await act(async () => renderer.unmount()); }
});

test('a late frontend listener becomes healthy from the native live-connection handshake', async () => {
  const bridge = harness({ studio_events: () => {} });
  const timers = controlledIntervals();
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    const status = () => renderer.root.findByProps({ 'aria-label': 'Studio status' });
    assert.match(renderedText(status()), /Connecting to Studio updates/);
    assert.notEqual(status().props['data-state'], 'online', 'subscribing alone does not establish a live service stream');
    await act(async () => { timers.tick(); await flush(); });
    assert.notEqual(status().props['data-state'], 'online', 'successful snapshot polls alone do not establish the stream');
    await act(async () => { bridge.emit('service.connected', {}); await flush(); });
    assert.match(renderedText(status()), /1 model serving/);
    assert.equal(status().props['data-state'], 'online');
    await act(async () => { bridge.emit('service.disconnected', { error: 'Event connection closed' }); });
    assert.notEqual(status().props['data-state'], 'online');
    assert.match(renderedText(status()), /reconnecting/);
  } finally { await act(async () => renderer.unmount()); }
});
