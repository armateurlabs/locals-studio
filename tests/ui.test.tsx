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
import { version as APP_VERSION } from '../package.json';
import { APP_BUILD_LABEL, APP_BUILD_TOOLTIP } from '../src/build-info';

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
          if (command === 'test_api') {
            calls.push({ method: command, params: args });
            if (overrides.test_api) return overrides.test_api(args);
            return { url: `${args.endpoint}/models`, status: 200, status_text: 'OK', elapsed_ms: 1, body: '{"object":"list","data":[]}' };
          }
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

function mockLogViewport() {
  let top = 0;
  const viewport = {
    scrollHeight: 1000,
    clientHeight: 200,
    get scrollTop() { return top; },
    set scrollTop(value: number) { top = Math.max(0, Math.min(value, Math.max(0, viewport.scrollHeight - viewport.clientHeight))); },
  };
  return viewport;
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
function libraryModel(root: TestRenderer.ReactTestInstance, name: string) {
  return root.findAllByType('button').find(node => String(node.props.className).split(' ').includes('library-model') && renderedText(node).includes(name))!;
}
function favoritesFilter(root: TestRenderer.ReactTestInstance) {
  return root.findAllByType('button').find(button => renderedText(button).trim() === 'Favorites only')!;
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

test('library favorite stars belong to model IDs and do not open details, change selection or send RPCs', async () => {
  const bridge = harness();
  const data = snapshot(); data.models.push({ ...model, id: 'other-model', name: 'Other model' });
  const actions: string[] = [];
  const callbacks = { onAction: async (method: string) => { actions.push(method); return {}; }, onSearch: () => {} };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<LibraryView snapshot={data} {...callbacks} />); });
  const star = (name: string, added = false) => renderer.root.findByProps({ 'aria-label': `${added ? 'Remove' : 'Add'} ${name} ${added ? 'from' : 'to'} favorites` });
  const click = async (button: TestRenderer.ReactTestInstance) => act(async () => { button.props.onClick({ stopPropagation() {}, preventDefault() {} }); });
  try {
    const baseline = bridge.calls.length;
    assert.equal(star(model.name).props['aria-pressed'], false);
    assert.equal(star(model.name).parent, libraryModel(renderer.root, model.name).parent, 'the star is a sibling of the model-selection button');
    await click(star(model.name));
    assert.equal(star(model.name, true).props['aria-pressed'], true);
    assert.deepEqual(JSON.parse(bridge.storage.get('studio.library.favoriteModelIds')!), [model.id]);
    assert.equal(renderer.root.findAllByProps({ className: 'model-inspector' }).length, 0, 'favoriting does not open model details');
    assert.equal(star('Other model').props['aria-pressed'], false, 'another ID remains independent');
    await click(libraryModel(renderer.root, 'Other model'));
    assert.equal(renderedText(renderer.root.findByProps({ className: 'inspector-model-heading' }).findByType('h2')), 'Other model');
    const renamed = { ...data, models: data.models.map(item => item.id === model.id ? { ...item, name: 'Renamed model' } : item) };
    await act(async () => { renderer.update(<LibraryView snapshot={renamed} {...callbacks} />); });
    assert.equal(star('Renamed model', true).props['aria-pressed'], true, 'renaming a model retains its favorite by ID');
    await click(star('Renamed model', true));
    assert.equal(star('Renamed model').props['aria-pressed'], false);
    assert.deepEqual(JSON.parse(bridge.storage.get('studio.library.favoriteModelIds')!), []);
    assert.equal(renderedText(renderer.root.findByProps({ className: 'inspector-model-heading' }).findByType('h2')), 'Other model', 'the star does not select a different inspector');
    assert.deepEqual(actions, []);
    assert.deepEqual(bridge.calls.slice(baseline), [], 'favorites stay entirely local');
  } finally { await act(async () => renderer.unmount()); }
});

test('library Favorites only composes with search and removing the last favorite has a Show all models escape', async () => {
  const bridge = harness();
  const data = snapshot(); data.models.push({ ...model, id: 'other-model', name: 'Other model' }, { ...model, id: 'third-model', name: 'Third model' });
  bridge.storage.set('studio.library.favoriteModelIds', JSON.stringify([model.id, 'other-model']));
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<LibraryView snapshot={data} onAction={async () => ({})} onSearch={() => {}} />); });
  const filter = () => favoritesFilter(renderer.root);
  const search = async (value: string) => act(async () => { renderer.root.findByProps({ 'aria-label': 'Filter local models' }).props.onChange({ target: { value } }); });
  const remove = async (name: string) => act(async () => { renderer.root.findByProps({ 'aria-label': `Remove ${name} from favorites` }).props.onClick({ stopPropagation() {}, preventDefault() {} }); });
  try {
    assert.equal(filter().props['aria-pressed'], false);
    await act(async () => { filter().props.onClick(); });
    assert.equal(filter().props['aria-pressed'], true);
    assert.equal(bridge.storage.get('studio.library.favoritesOnly'), 'true');
    assert.ok(libraryModel(renderer.root, model.name)); assert.ok(libraryModel(renderer.root, 'Other model'));
    assert.equal(libraryModel(renderer.root, 'Third model'), undefined);
    await search('oThEr');
    assert.ok(libraryModel(renderer.root, 'Other model'));
    assert.equal(libraryModel(renderer.root, model.name), undefined, 'favorites must also match the search');
    assert.equal(libraryModel(renderer.root, 'Third model'), undefined);
    await search('');
    await remove('Other model');
    assert.equal(libraryModel(renderer.root, 'Other model'), undefined, 'a removed favorite leaves the filtered view immediately');
    assert.ok(libraryModel(renderer.root, model.name));
    await remove(model.name);
    assert.equal(renderer.root.findAllByType('button').filter(button => String(button.props.className).split(' ').includes('library-model')).length, 0);
    const showAll = renderer.root.findAllByType('button').find(button => renderedText(button).trim() === 'Show all models');
    assert.ok(showAll, 'an empty favorites view offers a way back to all models');
    await act(async () => { showAll.props.onClick(); });
    assert.equal(filter().props['aria-pressed'], false);
    assert.equal(bridge.storage.get('studio.library.favoritesOnly'), 'false');
    assert.deepEqual(JSON.parse(bridge.storage.get('studio.library.favoriteModelIds')!), []);
    assert.ok(libraryModel(renderer.root, model.name)); assert.ok(libraryModel(renderer.root, 'Other model')); assert.ok(libraryModel(renderer.root, 'Third model'));
    assert.equal(bridge.calls.length, 0);
  } finally { await act(async () => renderer.unmount()); }
});

test('library favorites and Favorites only survive inspector tabs, navigation and a full app remount', async () => {
  const bridge = harness();
  bridge.data.models.push({ ...model, id: 'other-model', name: 'Other model' });
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  const navigate = async (name: string) => act(async () => { renderer.root.findByProps({ 'aria-label': name }).props.onClick(); await flush(); });
  const assertSaved = () => {
    assert.equal(favoritesFilter(renderer.root).props['aria-pressed'], true);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Remove Other model from favorites' }).props['aria-pressed'], true);
    assert.ok(libraryModel(renderer.root, 'Other model'));
    assert.equal(libraryModel(renderer.root, model.name), undefined);
  };
  try {
    await navigate('My models');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Add Other model to favorites' }).props.onClick({ stopPropagation() {}, preventDefault() {} }); });
    await act(async () => { favoritesFilter(renderer.root).props.onClick(); });
    await act(async () => { libraryModel(renderer.root, 'Other model').props.onClick(); });
    await act(async () => { renderer.root.findAllByType('button').find(button => renderedText(button).trim() === 'Server profiles')!.props.onClick(); await flush(); });
    assertSaved();
    await act(async () => { renderer.root.findAllByType('button').find(button => renderedText(button).trim() === 'Overview')!.props.onClick(); });
    assertSaved();
    await navigate('Chat'); await navigate('My models'); assertSaved();
    await act(async () => { renderer.unmount(); renderer = TestRenderer.create(<App />); await flush(); });
    await navigate('My models'); assertSaved();
    assert.deepEqual(JSON.parse(bridge.storage.get('studio.library.favoriteModelIds')!), ['other-model']);
    assert.equal(bridge.storage.get('studio.library.favoritesOnly'), 'true');
    assert.equal(bridge.calls.some(call => ['models.scan', 'models.remove', 'profiles.save', 'server.start', 'server.stop', 'config.set'].includes(call.method)), false);
  } finally { await act(async () => renderer.unmount()); }
});

test('an initially empty library snapshot does not erase saved favorite IDs before models load', async () => {
  const bridge = harness();
  bridge.storage.set('studio.library.favoriteModelIds', JSON.stringify([model.id]));
  bridge.storage.set('studio.library.favoritesOnly', 'true');
  const callbacks = { onAction: async () => ({}), onSearch: () => {} };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<LibraryView snapshot={structuredClone(EMPTY_SNAPSHOT)} {...callbacks} />); });
  try {
    assert.deepEqual(JSON.parse(bridge.storage.get('studio.library.favoriteModelIds')!), [model.id]);
    const loaded = snapshot(); loaded.models.push({ ...model, id: 'other-model', name: 'Other model' });
    await act(async () => { renderer.update(<LibraryView snapshot={loaded} {...callbacks} />); });
    assert.equal(favoritesFilter(renderer.root).props['aria-pressed'], true);
    assert.equal(renderer.root.findByProps({ 'aria-label': `Remove ${model.name} from favorites` }).props['aria-pressed'], true);
    assert.ok(libraryModel(renderer.root, model.name));
    assert.equal(libraryModel(renderer.root, 'Other model'), undefined);
    assert.deepEqual(JSON.parse(bridge.storage.get('studio.library.favoriteModelIds')!), [model.id]);
  } finally { await act(async () => renderer.unmount()); }
});

test('library favorites tolerate corrupt preferences and unavailable storage while retaining in-memory controls', async () => {
  for (const value of ['{invalid JSON', '{"unexpected":"object"}', 'null']) {
    const bridge = harness();
    bridge.storage.set('studio.library.favoriteModelIds', value);
    bridge.storage.set('studio.library.favoritesOnly', 'not-a-boolean');
    let renderer: TestRenderer.ReactTestRenderer;
    await act(async () => { renderer = TestRenderer.create(<LibraryView snapshot={snapshot()} onAction={async () => ({})} onSearch={() => {}} />); });
    try {
      assert.ok(libraryModel(renderer.root, model.name));
      assert.equal(renderer.root.findByProps({ 'aria-label': `Add ${model.name} to favorites` }).props['aria-pressed'], false);
      assert.equal(favoritesFilter(renderer.root).props['aria-pressed'], false);
    } finally { await act(async () => renderer.unmount()); }
  }
  for (const unavailable of ['methods', 'access']) {
    harness();
    const descriptor = Object.getOwnPropertyDescriptor(globalThis, 'localStorage')!;
    const fail = () => { throw new Error('Storage is unavailable'); };
    Object.defineProperty(globalThis, 'localStorage', unavailable === 'access'
      ? { configurable: true, get: fail }
      : { configurable: true, writable: true, value: { getItem: fail, setItem: fail } });
    let renderer: TestRenderer.ReactTestRenderer | undefined;
    try {
      await act(async () => { renderer = TestRenderer.create(<LibraryView snapshot={snapshot()} onAction={async () => ({})} onSearch={() => {}} />); });
      await act(async () => { renderer!.root.findByProps({ 'aria-label': `Add ${model.name} to favorites` }).props.onClick({ stopPropagation() {}, preventDefault() {} }); });
      assert.equal(renderer!.root.findByProps({ 'aria-label': `Remove ${model.name} from favorites` }).props['aria-pressed'], true);
      await act(async () => { favoritesFilter(renderer!.root).props.onClick(); });
      assert.equal(favoritesFilter(renderer!.root).props['aria-pressed'], true);
      assert.ok(libraryModel(renderer!.root, model.name));
    } finally {
      if (renderer) await act(async () => renderer!.unmount());
      Object.defineProperty(globalThis, 'localStorage', descriptor);
    }
  }
});

test('model details choose a ready profile, expose both actions above Configure server and never restart loading models', async () => {
  const bridge = harness();
  const data = snapshot();
  const stopped = { ...profile, alias: 'z-stopped' };
  const ready = { ...profile, id: 'ready-profile', alias: 'b-ready', selected: false };
  const loading = { ...profile, id: 'loading-profile', alias: 'a-loading', selected: false };
  data.profiles = [stopped, loading, ready];
  data.servers = [
    { ...data.servers[0], alias: stopped.alias, state: 'stopped' },
    { ...data.servers[0], profile_id: ready.id, alias: ready.alias, state: 'ready' },
    { ...data.servers[0], profile_id: loading.id, alias: loading.alias, state: 'loading' },
  ];
  const actions: any[] = [];
  const chosen: any[] = [];
  const callbacks = { onAction: async (method: string, params: any) => { actions.push({ method, params }); return { servers: [] }; }, onSearch: () => {}, onUseInChat: (value: any) => { chosen.push(value); } };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<LibraryView snapshot={data} {...callbacks} />); });
  try {
    await act(async () => { libraryModel(renderer.root, model.name).props.onClick(); });
    const picker = () => renderer.root.findByProps({ 'aria-label': 'Model action profile' });
    const start = () => renderer.root.findByProps({ 'aria-label': 'Start model' });
    const use = () => renderer.root.findByProps({ 'aria-label': 'Use in new chat' });
    assert.equal(picker().props.value, ready.id, 'a ready profile takes precedence over a selected stopped one');
    assert.equal(start().props.disabled, true);
    const inspectorButtons = renderer.root.findByProps({ className: 'model-inspector' }).findAllByType('button');
    const configure = inspectorButtons.findIndex(button => /Configure server/.test(renderedText(button)));
    assert.ok(inspectorButtons.indexOf(start()) < configure && inspectorButtons.indexOf(use()) < configure);
    await act(async () => { use().props.onClick(); await flush(); });
    assert.equal(chosen[0].id, ready.id);
    assert.deepEqual(actions, [], 'using an existing profile only opens a chat');
    await act(async () => { picker().props.onChange({ target: { value: loading.id } }); });
    assert.equal(start().props.disabled, true, 'a loading profile cannot be started again');
    await act(async () => { use().props.onClick(); await flush(); });
    assert.equal(chosen.at(-1).id, loading.id);
    await act(async () => { picker().props.onChange({ target: { value: stopped.id } }); });
    assert.equal(start().props.disabled, false);
    await act(async () => { start().props.onClick(); await flush(); });
    assert.deepEqual(bridge.calls.filter(call => call.method === 'memory.estimate').map(call => call.params), [{ id: stopped.id }]);
    assert.deepEqual(actions, [{ method: 'server.start', params: { id: stopped.id } }]);
  } finally { await act(async () => renderer.unmount()); }
});

test('model actions create a unique unselected profile only after a click and reuse it for chat and start', async () => {
  const bridge = harness();
  const data = snapshot();
  data.models.push({ ...model, id: 'other-model', name: 'Other model' });
  data.profiles = [{ ...profile, model_id: 'other-model', alias: 'test-model' }];
  data.servers = [];
  const actions: any[] = [];
  const chosen: any[] = [];
  const onAction = async (method: string, params: any) => { actions.push({ method, params }); return method === 'profiles.save' ? structuredClone(params.profile) : { servers: [] }; };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<LibraryView snapshot={data} onAction={onAction} onSearch={() => {}} onUseInChat={value => chosen.push(value)} />); });
  try {
    await act(async () => { libraryModel(renderer.root, model.name).props.onClick(); });
    assert.deepEqual(actions, [], 'opening model details does not create a profile or start a worker');
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Model action profile' }).length, 0);
    const use = () => renderer.root.findByProps({ 'aria-label': 'Use in new chat' });
    await act(async () => { use().props.onClick(); await flush(); });
    const saved = actions.find(action => action.method === 'profiles.save').params.profile;
    assert.equal(saved.model_id, model.id);
    assert.equal(saved.selected, false, 'a quick action does not change the global Start selected scope');
    assert.deepEqual(saved.options, {});
    assert.deepEqual(saved.env, {});
    assert.match(saved.alias, /^[A-Za-z0-9._-]+$/);
    assert.notEqual(saved.alias, 'test-model', 'an alias already used by another model remains reserved');
    assert.equal(chosen[0].id, saved.id);
    await act(async () => { use().props.onClick(); await flush(); });
    assert.equal(actions.filter(action => action.method === 'profiles.save').length, 1);
    assert.equal(chosen[1].id, saved.id);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Start model' }).props.onClick(); await flush(); });
    assert.deepEqual(bridge.calls.filter(call => call.method === 'memory.estimate').map(call => call.params), [{ id: saved.id }]);
    assert.deepEqual(actions.filter(action => action.method === 'server.start').map(action => action.params), [{ id: saved.id }]);
    assert.equal(actions.filter(action => action.method === 'profiles.save').length, 1);
  } finally { await act(async () => renderer.unmount()); }
});

test('model action defaults prefer loading, then selected profiles, then stable alias order', async () => {
  harness();
  for (const priority of ['loading', 'selected', 'alias']) {
    const data = snapshot();
    const selected = { ...profile, id: 'selected-profile', alias: 'z-selected', selected: priority !== 'alias' };
    const alpha = { ...profile, id: 'alpha-profile', alias: 'alpha', selected: false };
    const loading = { ...profile, id: 'loading-profile', alias: 'loading', selected: false };
    data.profiles = [selected, loading, alpha];
    data.servers = priority === 'loading' ? [{ ...data.servers[0], profile_id: loading.id, alias: loading.alias, state: 'starting' }] : [];
    let renderer: TestRenderer.ReactTestRenderer;
    await act(async () => { renderer = TestRenderer.create(<LibraryView snapshot={data} onAction={async () => ({})} onSearch={() => {}} onUseInChat={() => {}} />); });
    try {
      await act(async () => { libraryModel(renderer.root, model.name).props.onClick(); });
      const expected = priority === 'loading' ? loading.id : priority === 'selected' ? selected.id : alpha.id;
      assert.equal(renderer.root.findByProps({ 'aria-label': 'Model action profile' }).props.value, expected, `${priority} determines the initial choice regardless of stored profile order`);
    } finally { await act(async () => renderer.unmount()); }
  }
});

test('model Start checks memory once, supports cancellation and only overrides the budget after Start anyway', async () => {
  const pending = deferred<any>();
  const estimate = { total_estimated_bytes: 100, budget_bytes: 80, requires_override: true, warnings: [], note: 'Review the configured load.' };
  const bridge = harness({ 'memory.estimate': () => pending.promise });
  const data = snapshot(); data.servers[0].state = 'stopped';
  const actions: any[] = [];
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<LibraryView snapshot={data} onAction={async (method, params) => { actions.push({ method, params }); return { servers: [] }; }} onSearch={() => {}} />); });
  const start = () => renderer.root.findByProps({ 'aria-label': 'Start model' });
  const review = () => renderer.root.findByProps({ role: 'dialog', 'aria-label': 'Review memory usage' });
  try {
    await act(async () => { libraryModel(renderer.root, model.name).props.onClick(); });
    const original = start();
    await act(async () => { original.props.onClick(); original.props.onClick(); await flush(); });
    assert.equal(start().props.disabled, true);
    assert.equal(bridge.calls.filter(call => call.method === 'memory.estimate').length, 1, 'synchronous double clicks share one preflight');
    await act(async () => { pending.resolve(estimate); await flush(); });
    assert.deepEqual(actions, []);
    await act(async () => { review().findAllByType('button').find(button => renderedText(button).trim() === 'Cancel')!.props.onClick(); });
    assert.equal(renderer.root.findAllByProps({ role: 'dialog', 'aria-label': 'Review memory usage' }).length, 0);
    assert.deepEqual(actions, []);
    await act(async () => { start().props.onClick(); await flush(); });
    await act(async () => { review().findAllByType('button').find(button => renderedText(button).trim() === 'Start anyway')!.props.onClick(); await flush(); });
    assert.deepEqual(actions, [{ method: 'server.start', params: { id: profile.id, allow_memory_overcommit: true } }]);
  } finally { await act(async () => renderer.unmount()); }
});

test('model actions show profile and start failures, allow retry, and discard preflights after changing models', async () => {
  let failSave = true;
  let failStart = true;
  let holdEstimate = false;
  const pending = deferred<any>();
  harness({ 'memory.estimate': () => holdEstimate ? pending.promise : { requires_override: false } });
  const data = snapshot(); data.profiles = []; data.servers = [];
  data.models.push({ ...model, id: 'other-model', name: 'Other model' });
  const actions: any[] = [];
  const chosen: any[] = [];
  const onAction = async (method: string, params: any) => {
    actions.push({ method, params });
    if (method === 'profiles.save') { if (failSave) throw new Error('Could not save quick profile'); return params.profile; }
    if (method === 'server.start' && failStart) throw new Error('Could not start selected model');
    return { servers: [] };
  };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<LibraryView snapshot={data} onAction={onAction} onSearch={() => {}} onUseInChat={value => chosen.push(value)} />); });
  try {
    await act(async () => { libraryModel(renderer.root, model.name).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Use in new chat' }).props.onClick(); await flush(); });
    assert.match(renderedText(renderer.root), /Could not save quick profile/);
    assert.equal(chosen.length, 0);
    failSave = false;
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Use in new chat' }).props.onClick(); await flush(); });
    assert.equal(chosen.length, 1);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Start model' }).props.onClick(); await flush(); });
    assert.match(renderedText(renderer.root), /Could not start selected model/);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Start model' }).props.disabled, false);
    failStart = false; holdEstimate = true;
    const starts = actions.filter(action => action.method === 'server.start').length;
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Start model' }).props.onClick(); await flush(); });
    await act(async () => { libraryModel(renderer.root, 'Other model').props.onClick(); });
    await act(async () => { pending.resolve({ requires_override: true, warnings: [], total_estimated_bytes: 100, budget_bytes: 80 }); await flush(); });
    assert.equal(actions.filter(action => action.method === 'server.start').length, starts, 'a stale preflight cannot launch the previous model');
    assert.equal(renderer.root.findAllByProps({ role: 'dialog', 'aria-label': 'Review memory usage' }).length, 0);
  } finally { await act(async () => renderer.unmount()); }
});

test('model actions forget a quick-created profile observed and then deleted while the configuration tab is open', async () => {
  const pending = deferred<any>();
  harness({ 'memory.estimate': () => pending.promise });
  const data = snapshot(); data.profiles = []; data.servers = [];
  const saved: any[] = [];
  const chosen: any[] = [];
  const starts: any[] = [];
  const callbacks = {
    onAction: async (method: string, params: any) => {
      if (method === 'profiles.save') { saved.push(structuredClone(params.profile)); return params.profile; }
      if (method === 'server.start') starts.push(params);
      return {};
    },
    onSearch: () => {}, onUseInChat: (value: any) => { chosen.push(value); },
  };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<LibraryView snapshot={data} {...callbacks} />); });
  const tab = (name: string) => renderer.root.findAllByType('button').find(button => renderedText(button).trim() === name)!;
  try {
    await act(async () => { libraryModel(renderer.root, model.name).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Start model' }).props.onClick(); await flush(); });
    assert.equal(saved.length, 1);
    const deleted = saved[0];
    await act(async () => { tab('Server profiles').props.onClick(); await flush(); });
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Start model' }).length, 0, 'the quick action component is unmounted');
    await act(async () => { renderer.update(<LibraryView snapshot={{ ...data, profiles: [deleted] }} {...callbacks} />); await flush(); });
    await act(async () => { renderer.update(<LibraryView snapshot={{ ...data, profiles: [] }} {...callbacks} />); await flush(); });
    await act(async () => { tab('Overview').props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Use in new chat' }).props.onClick(); await flush(); });
    assert.equal(saved.length, 2, 'a profile removed after appearing in a snapshot must be recreated');
    assert.notEqual(saved[1].id, deleted.id);
    assert.equal(chosen.length, 1);
    assert.equal(chosen[0].id, saved[1].id, 'the chat must not use the deleted cached profile');
    await act(async () => { pending.resolve({ requires_override: false }); await flush(); });
    assert.deepEqual(starts, [], 'the preflight from the unmounted quick action remains cancelled');
  } finally { pending.resolve({ requires_override: false }); await act(async () => renderer.unmount()); }
});

test('model Reload and Stop follow worker state and Stop targets only the chosen profile once', async () => {
  const bridge = harness();
  const data = snapshot();
  const second = { ...profile, id: 'second-profile', alias: 'second', selected: false };
  data.profiles.push(second); data.servers.push({ ...data.servers[0], profile_id: second.id, alias: second.alias });
  const stopped = deferred<any>();
  const actions: any[] = [];
  const onAction = async (method: string, params: any) => { actions.push({ method, params }); return method === 'server.stop' ? stopped.promise : {}; };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<LibraryView snapshot={data} onAction={onAction} onSearch={() => {}} />); });
  const reload = () => renderer.root.findByProps({ 'aria-label': 'Reload model' });
  const stop = () => renderer.root.findByProps({ 'aria-label': 'Stop model' });
  try {
    await act(async () => { libraryModel(renderer.root, model.name).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Model action profile' }).props.onChange({ target: { value: profile.id } }); });
    for (const state of ['ready', 'running', 'starting', 'loading', 'draining', 'stopping', 'stopped', 'error']) {
      data.servers[0].state = state;
      await act(async () => { renderer.update(<LibraryView snapshot={structuredClone(data)} onAction={onAction} onSearch={() => {}} />); });
      assert.equal(reload().props.disabled, !['ready', 'running'].includes(state), `Reload: ${state}`);
      assert.equal(stop().props.disabled, !['ready', 'running', 'starting', 'loading'].includes(state), `Stop: ${state}`);
    }
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Model action profile' }).props.onChange({ target: { value: second.id } }); });
    const stopOnce = stop().props.onClick;
    await act(async () => { stopOnce(); stopOnce(); await flush(); });
    assert.deepEqual(actions, [{ method: 'server.stop', params: { id: second.id } }]);
    assert.equal(stop().props.disabled, true);
    assert.equal(reload().props.disabled, true);
    assert.equal(bridge.calls.some(call => call.method === 'memory.estimate'), false, 'Stop needs no memory preflight');
    await act(async () => { stopped.resolve({ servers: [{ profile_id: second.id, state: 'stopped' }] }); await flush(); });
    assert.equal(actions.some(action => action.method === 'server.start'), false);
  } finally { stopped.resolve({}); await act(async () => renderer.unmount()); }
});

test('model Stop immediately overrides a stale ready snapshot until a newly started worker replaces it', async () => {
  harness();
  const data = snapshot();
  data.servers[0] = { ...data.servers[0], state: 'ready', pid: 1111, port: 51111, started_at: '2026-10-27T12:00:00Z' };
  const oldServer = structuredClone(data.servers[0]);
  const actions: any[] = [];
  const callbacks = {
    onAction: async (method: string, params: any) => {
      actions.push({ method, params });
      return { servers: [{ ...oldServer, state: 'stopped' }] };
    }, onSearch: () => {},
  };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<LibraryView snapshot={data} {...callbacks} />); });
  const start = () => renderer.root.findByProps({ 'aria-label': 'Start model' });
  const reload = () => renderer.root.findByProps({ 'aria-label': 'Reload model' });
  const stop = () => renderer.root.findByProps({ 'aria-label': 'Stop model' });
  try {
    await act(async () => { libraryModel(renderer.root, model.name).props.onClick(); });
    assert.equal(start().props.disabled, true);
    assert.equal(stop().props.disabled, false);
    await act(async () => { stop().props.onClick(); await flush(); });
    assert.equal(data.servers[0].state, 'ready', 'the input snapshot deliberately remains stale');
    assert.equal(stop().props.disabled, true, 'the stop result takes effect before the next poll');
    assert.equal(reload().props.disabled, true);
    assert.equal(start().props.disabled, false);
    await act(async () => { renderer.update(<LibraryView snapshot={structuredClone(data)} {...callbacks} />); });
    assert.equal(stop().props.disabled, true, 'a delayed copy of the same old snapshot cannot undo the stop');
    const refreshed = { ...data, servers: [{ ...oldServer, state: 'ready', pid: 2222, port: 51112, started_at: '2026-10-27T12:01:00Z' }] };
    await act(async () => { renderer.update(<LibraryView snapshot={refreshed} {...callbacks} />); });
    assert.equal(stop().props.disabled, false, 'a fresh worker identity replaces the temporary stopped result');
    assert.equal(reload().props.disabled, false);
    assert.equal(start().props.disabled, true);
    assert.deepEqual(actions, [{ method: 'server.stop', params: { id: profile.id } }]);
  } finally { await act(async () => renderer.unmount()); }
});

test('model Reload checks memory before stopping and waits for the chosen stop to finish before starting', async () => {
  const memory = deferred<any>();
  const stopping = deferred<any>();
  const bridge = harness({ 'memory.estimate': () => memory.promise });
  const data = snapshot();
  const second = { ...profile, id: 'second-profile', alias: 'second', selected: false };
  data.profiles.push(second); data.servers.push({ ...data.servers[0], profile_id: second.id, alias: second.alias });
  const actions: any[] = [];
  const onAction = async (method: string, params: any) => {
    actions.push({ method, params });
    return method === 'server.stop' ? stopping.promise : { servers: [{ profile_id: second.id, state: 'ready' }] };
  };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<LibraryView snapshot={data} onAction={onAction} onSearch={() => {}} />); });
  try {
    await act(async () => { libraryModel(renderer.root, model.name).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Model action profile' }).props.onChange({ target: { value: second.id } }); });
    const reload = renderer.root.findByProps({ 'aria-label': 'Reload model' }).props.onClick;
    await act(async () => { reload(); reload(); await flush(); });
    assert.deepEqual(bridge.calls.filter(call => call.method === 'memory.estimate').map(call => call.params), [{ id: second.id }]);
    assert.deepEqual(actions, [], 'a running worker stays untouched until memory is checked');
    await act(async () => { memory.resolve({ requires_override: false }); await flush(); });
    assert.deepEqual(actions, [{ method: 'server.stop', params: { id: second.id } }]);
    await act(async () => { stopping.resolve({ servers: [{ profile_id: second.id, state: 'stopped' }] }); await flush(); });
    assert.deepEqual(actions, [{ method: 'server.stop', params: { id: second.id } }, { method: 'server.start', params: { id: second.id } }]);
  } finally { memory.resolve({ requires_override: false }); stopping.resolve({}); await act(async () => renderer.unmount()); }
});

test('model Reload cancellation preserves the worker and a failed stop never starts it', async () => {
  harness({ 'memory.estimate': () => ({ requires_override: true, total_estimated_bytes: 100, budget_bytes: 80, warnings: [] }) });
  let failStop = true;
  const actions: any[] = [];
  const onAction = async (method: string, params: any) => {
    actions.push({ method, params });
    if (method === 'server.stop' && failStop) throw new Error('Selected worker could not stop');
    return { servers: [{ profile_id: profile.id, state: method === 'server.stop' ? 'stopped' : 'ready' }] };
  };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<LibraryView snapshot={snapshot()} onAction={onAction} onSearch={() => {}} />); });
  const review = () => renderer.root.findByProps({ role: 'dialog', 'aria-label': 'Review memory usage' });
  const reload = async () => act(async () => { renderer.root.findByProps({ 'aria-label': 'Reload model' }).props.onClick(); await flush(); });
  const accept = async () => act(async () => { const start = review().findAllByType('button').find(button => renderedText(button).trim() === 'Start anyway')!; start.props.onClick(); start.props.onClick(); await flush(); });
  try {
    await act(async () => { libraryModel(renderer.root, model.name).props.onClick(); });
    await reload();
    await act(async () => { review().findAllByType('button').find(button => renderedText(button).trim() === 'Cancel')!.props.onClick(); });
    assert.deepEqual(actions, [], 'cancelling a reload review does not stop the current worker');
    await reload(); await accept();
    assert.deepEqual(actions, [{ method: 'server.stop', params: { id: profile.id } }]);
    assert.match(renderedText(renderer.root.findByProps({ role: 'alert' })), /Selected worker could not stop/);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Reload model' }).props.disabled, false, 'a failed stop can be retried');
    failStop = false;
    await reload(); await accept();
    assert.deepEqual(actions.slice(1), [{ method: 'server.stop', params: { id: profile.id } }, { method: 'server.start', params: { id: profile.id, allow_memory_overcommit: true } }]);
  } finally { await act(async () => renderer.unmount()); }
});

test('model Reload discards pending memory checks after profile changes or unmounting', async () => {
  const first = deferred<any>(); const second = deferred<any>();
  let estimates = 0;
  harness({ 'memory.estimate': () => estimates++ === 0 ? first.promise : second.promise });
  const data = snapshot();
  const other = { ...profile, id: 'other-profile', alias: 'other' };
  data.profiles.push(other); data.servers.push({ ...data.servers[0], profile_id: other.id, alias: other.alias });
  const actions: any[] = [];
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<LibraryView snapshot={data} onAction={async (method, params) => { actions.push({ method, params }); return {}; }} onSearch={() => {}} />); });
  try {
    await act(async () => { libraryModel(renderer.root, model.name).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Reload model' }).props.onClick(); await flush(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Model action profile' }).props.onChange({ target: { value: profile.id } }); });
    await act(async () => { first.resolve({ requires_override: true }); await flush(); });
    assert.deepEqual(actions, []);
    assert.equal(renderer.root.findAllByProps({ role: 'dialog', 'aria-label': 'Review memory usage' }).length, 0);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Reload model' }).props.onClick(); await flush(); renderer.unmount(); });
    await act(async () => { second.resolve({ requires_override: false }); await flush(); });
    assert.deepEqual(actions, [], 'an unmounted reload cannot stop or start a worker');
  } finally { first.resolve({ requires_override: false }); second.resolve({ requires_override: false }); }
});

test('Use in new chat selects the requested stopped model without starting it or disturbing an older streaming conversation', async () => {
  const bridge = harness();
  const targetModel = { ...model, id: 'target-model', name: 'Target model' };
  const targetProfile = { ...profile, id: 'target-profile', model_id: targetModel.id, alias: 'z-target', selected: false };
  bridge.data.models.push(targetModel); bridge.data.profiles.push(targetProfile);
  bridge.data.servers.push({ ...bridge.data.servers[0], profile_id: targetProfile.id, model_id: targetModel.id, alias: targetProfile.alias, state: 'stopped' });
  bridge.data.conversations.push({ id: 'old-chat', title: 'Older streaming chat', model: profile.alias, thinking: true, messages: [{ role: 'user', content: 'Earlier question' }] });
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  const navigate = async (name: string) => act(async () => { renderer.root.findByProps({ 'aria-label': name }).props.onClick(); await flush(); });
  const useTarget = async () => {
    await navigate('My models');
    await act(async () => { libraryModel(renderer.root, targetModel.name).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Use in new chat' }).props.onClick(); await flush(); });
  };
  try {
    await act(async () => { conversationItem(renderer.root, 'Older streaming chat').props.onClick({}); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: 'Continue the older chat' } }); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Send message' }).props.onClick(); await flush(); bridge.resolveStart({ request_id: 'old-stream', conversation_id: 'old-chat' }); await flush(); });
    await act(async () => { bridge.emit('chat.delta', { request_id: 'old-stream', conversation_id: 'old-chat', delta: 'Preserved partial answer' }); renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: 'Keep the older draft' } }); });
    await useTarget();
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Chat model' }).props.value, targetProfile.alias, 'a stopped requested model is not replaced by the first ready model');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Message' }).props.value, '');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Send message' }).props.disabled, true);
    assert.match(renderedText(renderer.root.findByProps({ id: 'composer-availability' })), /z-target.*stopped/);
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Stop generating' }).length, 0, 'the new chat is empty and has no request');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Generation settings' }).props.onClick(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Thinking' }).props.value, '', 'a new chat does not inherit the old reasoning override');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: 'Unsent target draft' } }); });
    await useTarget();
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Chat model' }).props.value, targetProfile.alias);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Message' }).props.value, '', 'using the same model again still creates a new chat');
    await act(async () => { bridge.emit('chat.delta', { request_id: 'old-stream', conversation_id: 'old-chat', delta: ' still streaming' }); });
    await act(async () => { conversationItem(renderer.root, 'Older streaming chat').props.onClick({}); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Chat model' }).props.value, profile.alias);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Message' }).props.value, 'Keep the older draft');
    assert.ok(renderer.root.findAllByType(Markdown).some(node => node.props.children === 'Preserved partial answer still streaming'));
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Stop generating' }).length, 1);
    assert.equal(bridge.calls.filter(call => call.method === 'chat.start').length, 1);
    assert.equal(bridge.calls.some(call => ['server.start', 'server.stop', 'chat.cancel', 'profiles.save'].includes(call.method)), false);
    assert.equal(bridge.data.conversations[0].id, 'old-chat');
  } finally { await act(async () => renderer.unmount()); }
});

test('an explicit new chat wins over simultaneous recovery of a removed conversation and consumes its nonce only once', async () => {
  harness();
  const data = snapshot();
  const target = { ...profile, id: 'beta-profile', alias: 'beta', selected: false };
  data.profiles.push(target);
  data.servers.push({ ...data.servers[0], profile_id: target.id, alias: target.alias, state: 'stopped' });
  data.conversations.push({ id: 'removed-chat', title: 'Conversation being removed', model: profile.alias, thinking: true, messages: [{ role: 'user', content: 'Earlier message' }] });
  const actions: string[] = [];
  const callbacks = { onSend: async () => {}, onSave: async () => {}, onAction: async (method: string) => { actions.push(method); return {}; }, onServer: () => {} };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ChatView active snapshot={data} streams={{}} errors={{}} {...callbacks} />); await flush(); });
  try {
    await act(async () => { conversationItem(renderer.root, 'Conversation being removed').props.onClick({}); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Chat model' }).props.value, profile.alias);
    const afterRemoval = { ...data, conversations: [] };
    const request = { id: 'open-beta-once', model: target.alias };
    await act(async () => { renderer.update(<ChatView active snapshot={afterRemoval} streams={{}} errors={{}} newChatRequest={request} {...callbacks} />); await flush(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Chat model' }).props.value, target.alias, 'removed-conversation recovery cannot override the explicit requested model');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Message' }).props.value, '');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: 'Keep the new beta draft' } }); });
    await act(async () => { renderer.update(<ChatView active snapshot={structuredClone(afterRemoval)} streams={{}} errors={{}} newChatRequest={{ ...request }} {...callbacks} />); await flush(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Chat model' }).props.value, target.alias, 'a ready alternative does not replace the requested stopped model');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Message' }).props.value, 'Keep the new beta draft', 'the same nonce is not consumed again during refresh');
    assert.deepEqual(actions, []);
  } finally { await act(async () => renderer.unmount()); }
});

test('server view renders backend text logs and exposes keyboard resizing', async () => {
  harness();
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={snapshot()} onAction={async () => ({})} onLibrary={() => {}} />); await flush(); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Expand logs' }).props.onClick(); await flush(); });
    const lines = renderer.root.findByProps({ 'aria-label': 'Server logs', role: 'log' });
    const text = (node: any): string => typeof node === 'string' ? node : (node.children ?? []).map(text).join(' ');
    assert.match(text(lines), /Listening on localhost/);
    const handle = renderer.root.findByProps({ 'aria-label': 'Resize server logs' });
    const oldHeight = handle.props['aria-valuenow'];
    await act(async () => { handle.props.onKeyDown({ key: 'ArrowUp', preventDefault() {} }); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Resize server logs' }).props['aria-valuenow'], oldHeight + 20);
  } finally { await act(async () => renderer.unmount()); }
});

test('Test API uses the selected endpoint, formats JSON and copies the displayed response', async () => {
  const body = '{"object":"list","data":[{"id":"local-model"}]}';
  const bridge = harness({ test_api: ({ endpoint }) => ({ url: `${endpoint}/models`, status: 200, status_text: 'OK', elapsed_ms: 14, body }) });
  const data = snapshot();
  const first = 'http://127.0.0.1:8321/v1';
  const selected = 'http://192.168.1.20:8321/v1';
  data.api = { ...data.api, urls: [first, selected] };
  const copied: string[] = [];
  const oldNavigator = Object.getOwnPropertyDescriptor(globalThis, 'navigator');
  Object.defineProperty(globalThis, 'navigator', { configurable: true, value: { clipboard: { async writeText(value: string) { copied.push(value); } } } });
  window.setTimeout = (() => 0) as typeof window.setTimeout;
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={data} onAction={async () => ({})} onLibrary={() => {}} />); await flush(); });
  const dialog = () => renderer.root.findByProps({ role: 'dialog', 'aria-label': 'Test API' });
  const again = () => dialog().findAllByType('button').find(button => renderedText(button).trim() === 'Test again')!;
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'API endpoint' }).props.onChange({ target: { value: selected } }); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Test API' }).props.onClick(); await flush(); });
    assert.deepEqual(bridge.calls.filter(call => call.method === 'test_api').map(call => call.params), [{ endpoint: selected }]);
    assert.match(renderedText(dialog()), /200.*OK/);
    assert.ok(renderedText(dialog()).includes(`${selected}/models`));
    const pretty = JSON.stringify(JSON.parse(body), null, 2);
    assert.equal(dialog().findByType('pre').findByType('code').props.children, pretty);
    await act(async () => { await dialog().findByProps({ 'aria-label': 'Copy API response' }).props.onClick(); });
    assert.deepEqual(copied, [pretty]);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'API endpoint' }).props.onChange({ target: { value: first } }); });
    await act(async () => { again().props.onClick(); await flush(); });
    assert.deepEqual(bridge.calls.filter(call => call.method === 'test_api').map(call => call.params), [{ endpoint: selected }, { endpoint: selected }], 'Test again uses the endpoint captured by the open modal');
    assert.equal(bridge.calls.some(call => ['server.start', 'server.stop', 'chat.start'].includes(call.method)), false, 'testing the API does not start models or generate output');
  } finally {
    await act(async () => renderer.unmount());
    if (oldNavigator) Object.defineProperty(globalThis, 'navigator', oldNavigator); else delete (globalThis as any).navigator;
  }
});

test('Test API displays HTTP error bodies as JSON and leaves non-JSON response text intact', async () => {
  const bodies = ['{"error":{"message":"Model unavailable"}}', '<html>upstream failed\n  retry later</html>'];
  let attempts = 0;
  harness({ test_api: ({ endpoint }) => ({ url: `${endpoint}/models`, status: 500, status_text: 'Internal Server Error', elapsed_ms: 3, body: bodies[attempts++] }) });
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={snapshot()} onAction={async () => ({})} onLibrary={() => {}} />); await flush(); });
  const dialog = () => renderer.root.findByProps({ role: 'dialog', 'aria-label': 'Test API' });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Test API' }).props.onClick(); await flush(); });
    assert.match(renderedText(dialog()), /500.*Internal Server Error/);
    assert.equal(dialog().findByType('pre').findByType('code').props.children, JSON.stringify(JSON.parse(bodies[0]), null, 2), 'an HTTP error remains a readable response');
    assert.equal(dialog().findByProps({ 'aria-label': 'Copy API response' }).props.disabled, false);
    await act(async () => { dialog().findAllByType('button').find(button => renderedText(button).trim() === 'Test again')!.props.onClick(); await flush(); });
    assert.equal(dialog().findByType('pre').findByType('code').props.children, bodies[1], 'HTML and whitespace are not parsed, reformatted or interpreted as markup');
    assert.equal(attempts, 2);
  } finally { await act(async () => renderer.unmount()); }
});

test('Test API explains a network failure and can retry successfully in the same modal', async () => {
  let attempts = 0;
  harness({ test_api: ({ endpoint }) => {
    if (attempts++ === 0) throw new Error('Connection refused at the selected endpoint');
    return { url: `${endpoint}/models`, status: 200, status_text: 'OK', elapsed_ms: 2, body: '{"data":[]}' };
  } });
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={snapshot()} onAction={async () => ({})} onLibrary={() => {}} />); await flush(); });
  const dialog = () => renderer.root.findByProps({ role: 'dialog', 'aria-label': 'Test API' });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Test API' }).props.onClick(); await flush(); });
    assert.match(renderedText(dialog().findByProps({ role: 'alert' })), /Connection refused/);
    const retry = dialog().findAllByType('button').find(button => renderedText(button).trim() === 'Test again')!;
    assert.equal(retry.props.disabled, false);
    await act(async () => { retry.props.onClick(); await flush(); });
    assert.equal(dialog().findAllByProps({ role: 'alert' }).length, 0);
    assert.equal(dialog().findByType('pre').findByType('code').props.children, JSON.stringify({ data: [] }, null, 2));
    assert.equal(attempts, 2);
  } finally { await act(async () => renderer.unmount()); }
});

test('closing a pending API test ignores its late response without overwriting a newly opened endpoint test', async () => {
  const oldResult = deferred<any>();
  const newResult = deferred<any>();
  const first = 'http://127.0.0.1:8321/v1';
  const second = 'http://192.168.1.20:8321/v1';
  const bridge = harness({ test_api: ({ endpoint }) => endpoint === first ? oldResult.promise : newResult.promise });
  const data = snapshot(); data.api = { ...data.api, urls: [first, second] };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={data} onAction={async () => ({})} onLibrary={() => {}} />); await flush(); });
  const dialog = () => renderer.root.findByProps({ role: 'dialog', 'aria-label': 'Test API' });
  const open = () => renderer.root.findAllByType('button').find(button => button.props['aria-label'] === 'Test API')!;
  try {
    await act(async () => { open().props.onClick(); await flush(); });
    assert.ok(dialog().findAllByProps({ 'aria-label': 'Working' }).length > 0);
    assert.equal(dialog().findAllByType('button').find(button => renderedText(button).trim() === 'Test again')!.props.disabled, true);
    await act(async () => { dialog().findByProps({ 'aria-label': 'Close dialog' }).props.onClick(); });
    assert.equal(renderer.root.findAllByProps({ role: 'dialog', 'aria-label': 'Test API' }).length, 0);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'API endpoint' }).props.onChange({ target: { value: second } }); });
    await act(async () => { open().props.onClick(); await flush(); });
    await act(async () => { newResult.resolve({ url: `${second}/models`, status: 200, status_text: 'OK', elapsed_ms: 2, body: 'New endpoint response' }); await flush(); });
    assert.equal(dialog().findByType('pre').findByType('code').props.children, 'New endpoint response');
    await act(async () => { oldResult.resolve({ url: `${first}/models`, status: 500, status_text: 'Old error', elapsed_ms: 500, body: 'Stale old endpoint response' }); await flush(); });
    assert.equal(dialog().findByType('pre').findByType('code').props.children, 'New endpoint response');
    assert.doesNotMatch(renderedText(dialog()), /Stale old|Old error/);
    assert.deepEqual(bridge.calls.filter(call => call.method === 'test_api').map(call => call.params), [{ endpoint: first }, { endpoint: second }]);
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
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Expand logs' }).props.onClick(); await flush(); });
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
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Expand logs' }).props.onClick(); await flush(); });
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

test('About and footer show the build identity with the package version, bundled license and Armateur Labs link', async () => {
  const bridge = harness();
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    const footerBuild = renderer.root.findByProps({ className: 'version' });
    assert.equal(footerBuild.children.join(''), APP_BUILD_LABEL, 'the footer shows the exact tag or commit label without an added prefix');
    assert.equal(footerBuild.props.title, APP_BUILD_TOOLTIP);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Settings' }).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ role: 'tab', 'aria-label': 'About Locals Studio' }).props.onClick(); });
    const about = () => renderer.root.findByProps({ role: 'dialog', 'aria-label': 'About Locals Studio' });
    const aboutBuild = about().findByProps({ className: 'about-product' }).findByProps({ className: 'tag' });
    assert.equal(aboutBuild.children.join(''), `Build ${APP_BUILD_LABEL}`, 'About identifies the same build');
    assert.equal(aboutBuild.props.title, APP_BUILD_TOOLTIP);
    assert.equal(about().findByProps({ 'aria-label': 'Application version' }).children.join(''), `Version ${APP_VERSION}`, 'the package version remains available separately');
    const license = await readFile(new URL('../LICENSE', import.meta.url), 'utf8');
    assert.equal(renderedText(about().findByProps({ 'aria-label': 'MIT License text' })).trim(), license.trim(), 'the offline license matches the repository');
    assert.equal(about().findByProps({ 'aria-label': 'Visit Armateur Labs' }).props.href, 'https://armateurlabs.fr');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Close dialog' }).props.onClick(); });
    await act(async () => { bridge.emit('desktop.about', {}); await flush(); });
    assert.ok(about(), 'the native About action opens the same section');
    assert.equal(bridge.calls.some(call => ['server.start', 'server.stop', 'config.set'].includes(call.method)), false);
  } finally { await act(async () => renderer.unmount()); }
});

test('the native Settings shortcut opens General from every view and preserves drafts and unsaved settings', async () => {
  const bridge = harness();
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: 'Keep my message draft' } }); });
    for (const view of ['Chat', 'Model server', 'My models', 'Discover']) {
      await act(async () => { renderer.root.findByProps({ 'aria-label': view }).props.onClick(); await flush(); });
      await act(async () => { bridge.emit('desktop.settings', {}); await flush(); });
      const dialog = () => renderer.root.findByProps({ role: 'dialog', 'aria-label': 'Studio settings' });
      assert.equal(renderer.root.findAllByProps({ role: 'dialog', 'aria-label': 'Studio settings' }).length, 1);
      assert.ok(dialog().findByProps({ id: 'settings-general-panel', role: 'tabpanel' }));
      await act(async () => { dialog().findByProps({ 'aria-label': 'Allowed hosts' }).props.onChange({ target: { value: 'unsaved.example' } }); });
      await act(async () => { bridge.emit('desktop.settings', {}); await flush(); });
      assert.equal(dialog().findByProps({ 'aria-label': 'Allowed hosts' }).props.value, 'unsaved.example');
      await act(async () => { dialog().findByProps({ role: 'tab', 'aria-label': 'About Locals Studio' }).props.onClick(); });
      await act(async () => { bridge.emit('desktop.settings', {}); await flush(); });
      assert.equal(dialog().findByProps({ 'aria-label': 'Allowed hosts' }).props.value, 'unsaved.example', 'returning from About keeps the settings draft');
      await act(async () => { dialog().findByProps({ 'aria-label': 'Close dialog' }).props.onClick(); });
    }
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Chat' }).props.onClick(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Message' }).props.value, 'Keep my message draft');
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

test('chat Thinking sends explicit on and off overrides while Default removes a saved override', async () => {
  harness();
  const data = snapshot();
  data.conversations.push({ id: 'thinking-chat', title: 'Thinking preference', model: 'test', thinking: true, messages: [{ role: 'user', content: 'Earlier question' }] });
  const sent: any[][] = [];
  const actions: string[] = [];
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ChatView active snapshot={data} streams={{}} errors={{}} onSend={async (...args) => { sent.push(args); }} onSave={async () => {}} onAction={async method => { actions.push(method); return {}; }} onServer={() => {}} />); await flush(); });
  try {
    await act(async () => { conversationItem(renderer.root, 'Thinking preference').props.onClick({}); renderer.root.findByProps({ 'aria-label': 'Generation settings' }).props.onClick(); });
    const choice = () => renderer.root.findByProps({ 'aria-label': 'Thinking' });
    assert.equal(choice().props.value, 'on', 'a saved conversation restores its choice');
    assert.match(renderedText(renderer.root), /compatible models/);
    assert.match(renderedText(renderer.root), /Includes thinking and final answer tokens/);
    for (const value of ['', 'on', 'off', '']) {
      await act(async () => { choice().props.onChange({ target: { value } }); renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: `Question with ${value || 'default'}` } }); });
      await act(async () => { renderer.root.findByProps({ 'aria-label': 'Send message' }).props.onClick(); await flush(); });
      const [conversation, , selectedModel, parameters] = sent.at(-1)!;
      assert.equal(selectedModel, 'test');
      assert.equal(parameters.max_tokens, 1024, 'enabling thinking does not silently change the output budget');
      if (value) {
        assert.equal(conversation.thinking, value === 'on');
        assert.deepEqual(parameters.chat_template_kwargs, { enable_thinking: value === 'on' });
      } else {
        assert.equal(Object.hasOwn(conversation, 'thinking'), false, 'Default clears the persisted boolean');
        assert.equal(Object.hasOwn(parameters, 'chat_template_kwargs'), false, 'Default leaves the server default untouched');
      }
    }
    assert.equal(sent.length, 4);
    assert.deepEqual(actions, [], 'per-message thinking needs no server restart or profile change');
  } finally { await act(async () => renderer.unmount()); }
});

test('chat Thinking choices stay isolated between conversations and a new chat starts with Default', async () => {
  harness();
  const data = snapshot();
  data.conversations.push(
    { id: 'thinking-on', title: 'Thinking on', model: 'test', thinking: true, messages: [] },
    { id: 'thinking-off', title: 'Thinking off', model: 'test', thinking: false, messages: [] },
    { id: 'thinking-legacy', title: 'Thinking default', model: 'test', messages: [] },
  );
  let saved: any;
  const callbacks = { onSend: async (conversation: any) => { saved = structuredClone(conversation); }, onSave: async () => {}, onAction: async () => ({}), onServer: () => {} };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ChatView active snapshot={data} streams={{}} errors={{}} {...callbacks} />); await flush(); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Generation settings' }).props.onClick(); });
    const choice = () => renderer.root.findByProps({ 'aria-label': 'Thinking' });
    const select = async (title: string) => { await act(async () => { conversationItem(renderer.root, title).props.onClick({}); }); };
    assert.equal(choice().props.value, '');
    await select('Thinking on');
    assert.equal(choice().props.value, 'on');
    await act(async () => { choice().props.onChange({ target: { value: 'off' } }); });
    await select('Thinking off');
    assert.equal(choice().props.value, 'off');
    await act(async () => { choice().props.onChange({ target: { value: 'on' } }); });
    await select('Thinking on');
    assert.equal(choice().props.value, 'off', 'returning preserves this conversation’s unsent choice');
    await select('Thinking default');
    assert.equal(choice().props.value, '', 'legacy conversations use the server default');
    await select('Thinking off');
    assert.equal(choice().props.value, 'on', 'the other conversation keeps its own choice');
    await act(async () => { renderer.root.findByProps({ className: 'new-chat' }).props.onClick(); });
    assert.equal(choice().props.value, '', 'a new chat does not inherit another conversation’s override');
    await select('Thinking on');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Message' }).props.onChange({ target: { value: 'Save this choice' } }); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Send message' }).props.onClick(); await flush(); });
    assert.equal(saved.thinking, false);
    data.conversations = data.conversations.map(conversation => conversation.id === saved.id ? saved : conversation);
    await act(async () => { renderer.unmount(); renderer = TestRenderer.create(<ChatView active snapshot={data} streams={{}} errors={{}} {...callbacks} />); await flush(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Generation settings' }).props.onClick(); });
    await select('Thinking on');
    assert.equal(choice().props.value, 'off', 'reopening restores the boolean stored with the conversation');
  } finally { await act(async () => renderer.unmount()); }
});

test('chat labels preparation and observed reasoning separately from answer generation', async () => {
  harness();
  const data = snapshot();
  data.conversations.push({ id: 'phase-chat', title: 'Reply phases', model: 'test', thinking: false, messages: [{ role: 'user', content: 'A question' }] });
  const props = { active: true, snapshot: data, streams: { 'phase-chat': 'stream' }, errors: {}, onSend: async () => {}, onSave: async () => {}, onAction: async () => ({}), onServer: () => {} };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ChatView {...props} />); await flush(); });
  try {
    await act(async () => { conversationItem(renderer.root, 'Reply phases').props.onClick({}); });
    const phase = () => renderedText(renderer.root.findByProps({ className: 'generating' }));
    assert.match(phase(), /Preparing/);
    assert.doesNotMatch(phase(), /Thinking/);
    data.conversations[0].messages.push({ role: 'assistant', content: '', reasoning_content: 'Observed reasoning' });
    await act(async () => { renderer.update(<ChatView {...props} snapshot={structuredClone(data)} />); });
    assert.match(phase(), /Thinking/);
    assert.ok(renderer.root.findAllByType(Markdown).some(node => node.props.children === 'Observed reasoning'));
    data.conversations[0].messages.at(-1)!.content = 'The final answer';
    await act(async () => { renderer.update(<ChatView {...props} snapshot={structuredClone(data)} />); });
    assert.match(phase(), /Generating/);
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

test('Server favorite and running filters combine with selection and search without changing profiles or opening logs', async () => {
  const bridge = harness();
  bridge.storage.set('studio.library.favoriteModelIds', JSON.stringify([model.id]));
  const data = snapshot(); data.models.push({ ...model, id: 'other-model', name: 'Nonfavorite model' });
  data.profiles = [
    { ...profile, id: 'alpha', alias: 'alpha' },
    { ...profile, id: 'beta', alias: 'beta', selected: false },
    { ...profile, id: 'gamma', alias: 'gamma' },
    { ...profile, id: 'delta', alias: 'delta', model_id: 'other-model' },
    { ...profile, id: 'epsilon', alias: 'epsilon' },
  ];
  data.servers = data.profiles.map((entry, index) => ({ profile_id: entry.id, model_id: entry.model_id, alias: entry.alias, state: ['ready', 'running', 'loading', 'running', 'running'][index], active_requests: 0 }));
  const actions: any[] = [];
  const callbacks = { onAction: async (method: string, params: any) => { actions.push({ method, params }); return {}; }, onLibrary: () => {} };
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={data} {...callbacks} />); await flush(); });
  const shown = () => renderer.root.findAllByProps({ className: 'server-name' }).map(button => renderedText(button.findByType('strong')));
  const toggle = async (label: string, checked: boolean) => act(async () => { renderer.root.findByProps({ 'aria-label': label }).props.onChange({ target: { checked } }); });
  const search = async (value: string) => act(async () => { renderer.root.findByProps({ 'aria-label': 'Search local servers' }).props.onChange({ target: { value } }); });
  try {
    const focusedProfile = renderer.root.findByProps({ 'aria-label': 'Server logs profile' }).props.value;
    const filterOrder = renderer.root.findAllByType('input').map(input => input.props['aria-label']);
    assert.ok(filterOrder.indexOf('Show selected models only') < filterOrder.indexOf('Show favorite models only'));
    assert.ok(filterOrder.indexOf('Show favorite models only') < filterOrder.indexOf('Show running models only'));
    await toggle('Show favorite models only', true);
    assert.deepEqual(shown(), ['alpha', 'beta', 'epsilon', 'gamma'], 'all profiles of the same favorite model qualify');
    await toggle('Show running models only', true);
    assert.deepEqual(shown(), ['alpha', 'beta', 'epsilon'], 'both ready and running qualify');
    await toggle('Show selected models only', true);
    assert.deepEqual(shown(), ['alpha', 'epsilon']);
    await search('EPSILON');
    assert.deepEqual(shown(), ['epsilon'], 'search is combined with all three filters');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Server logs profile' }).props.value, focusedProfile, 'filtering does not change the inspected profile');
    await search('');
    for (const state of ['starting', 'loading', 'draining', 'stopping', 'stopped', 'error']) {
      data.servers[2].state = state;
      await act(async () => { renderer.update(<ServerView snapshot={structuredClone(data)} {...callbacks} />); });
      assert.deepEqual(shown(), ['alpha', 'epsilon'], `${state} is not running`);
    }
    await search('no matching server');
    await act(async () => { renderer.root.findAllByType('button').find(button => renderedText(button).trim() === 'Clear filters')!.props.onClick(); });
    assert.deepEqual(shown(), ['alpha', 'beta', 'delta', 'epsilon', 'gamma']);
    for (const [label, key] of [['Show selected models only', 'selectedOnly'], ['Show favorite models only', 'favoritesOnly'], ['Show running models only', 'runningOnly']]) {
      assert.equal(renderer.root.findByProps({ 'aria-label': label }).props.checked, false);
      assert.equal(bridge.storage.get(`studio.server.${key}`), 'false');
    }
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Search local servers' }).props.value, '');
    assert.deepEqual(data.profiles.map(entry => entry.selected), [true, false, true, true, true]);
    assert.deepEqual(actions, []);
    assert.equal(bridge.calls.some(call => call.method === 'server.logs'), false, 'filtering never opens or fetches logs');
  } finally { await act(async () => renderer.unmount()); }
});

test('Server favorite and running filters persist across navigation and app remounts and share favorites with My models', async () => {
  const bridge = harness();
  const otherModel = { ...model, id: 'other-model', name: 'Other model' };
  const otherProfile = { ...profile, id: 'other-profile', model_id: otherModel.id, alias: 'other', selected: false };
  bridge.data.models.push(otherModel); bridge.data.profiles.push(otherProfile);
  bridge.data.servers.push({ profile_id: otherProfile.id, model_id: otherModel.id, alias: otherProfile.alias, state: 'ready', active_requests: 0 });
  bridge.storage.set('studio.library.favoriteModelIds', JSON.stringify([model.id]));
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  const navigate = async (name: string) => act(async () => { renderer.root.findByProps({ 'aria-label': name }).props.onClick(); await flush(); });
  const shown = () => renderer.root.findAllByProps({ className: 'server-name' }).map(button => renderedText(button.findByType('strong')));
  const verifyFilters = () => {
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Show favorite models only' }).props.checked, true);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Show running models only' }).props.checked, true);
  };
  try {
    await navigate('Model server');
    for (const label of ['Show favorite models only', 'Show running models only']) await act(async () => { renderer.root.findByProps({ 'aria-label': label }).props.onChange({ target: { checked: true } }); });
    assert.deepEqual(shown(), ['test']);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Search local servers' }).props.onChange({ target: { value: 'test' } }); });
    await navigate('My models');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Add Other model to favorites' }).props.onClick(); });
    await navigate('Model server'); verifyFilters();
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Search local servers' }).props.value, 'test');
    assert.deepEqual(shown(), ['test']);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Clear server search' }).props.onClick(); });
    assert.deepEqual(shown(), ['other', 'test'], 'a favorite added in My models is read when returning to Server');
    await act(async () => { renderer.unmount(); renderer = TestRenderer.create(<App />); await flush(); });
    await navigate('Model server'); verifyFilters();
    assert.deepEqual(shown(), ['other', 'test']);
    assert.equal(bridge.storage.get('studio.server.favoritesOnly'), 'true');
    assert.equal(bridge.storage.get('studio.server.runningOnly'), 'true');
    assert.deepEqual(bridge.data.profiles.map(entry => entry.selected), [true, false]);
    assert.equal(bridge.calls.some(call => ['profiles.save', 'server.start', 'server.stop', 'config.set', 'server.logs'].includes(call.method)), false);
  } finally { await act(async () => renderer.unmount()); }
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

test('Server search matches aliases, model names and repositories and composes with selected-only across navigation', async () => {
  const bridge = harness();
  bridge.data.models = [{ ...model, name: 'Qwen Small', repo_id: 'mlx-community/Qwen-small' }, { ...model, id: 'm2', name: 'Llama Reasoner', repo_id: 'another-publisher/Llama-reasoner' }, { ...model, id: 'm3', name: 'Gemma Writer', repo_id: 'mlx-community/Gemma-writer' }];
  bridge.data.profiles = [{ ...profile, alias: 'Alpha chat' }, { ...profile, id: 'p2', model_id: 'm2', alias: 'Beta coding', selected: false }, { ...profile, id: 'p3', model_id: 'm3', alias: 'Gamma writing', selected: true }];
  bridge.data.servers = [{ ...bridge.data.servers[0], alias: 'Alpha chat', pid: 123 }, { profile_id: 'p3', model_id: 'm3', alias: 'Gamma writing', state: 'stopped', active_requests: 0 }];
  const before = structuredClone(bridge.data.profiles);
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  const rows = () => renderer.root.findAllByType('div').filter(node => String(node.props.className).split(' ').includes('server-row')).map(node => node.props['aria-label']);
  const search = async (value: string) => act(async () => { renderer.root.findByProps({ 'aria-label': 'Search local servers' }).props.onChange({ target: { value } }); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Model server' }).props.onClick(); await flush(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Logs for Beta coding' }).props.onClick(); await flush(); });
    await search('  bEtA  '); assert.deepEqual(rows(), ['Server Beta coding']);
    await search('REASONER'); assert.deepEqual(rows(), ['Server Beta coding']);
    await search('MLX-COMMUNITY'); assert.deepEqual(rows(), ['Server Alpha chat', 'Server Gamma writing']);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Show selected models only' }).props.onChange({ target: { checked: true } }); });
    await search('LLaMa');
    assert.deepEqual(rows(), []);
    assert.match(renderedText(renderer.root.findByProps({ className: 'server-view' })), /No matching servers/);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Server logs profile' }).props.value, 'p2', 'filtering retains the inspected hidden profile');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Clear server search' }).props.onClick(); });
    assert.deepEqual(rows(), ['Server Alpha chat', 'Server Gamma writing']);
    await search('SMALL'); assert.deepEqual(rows(), ['Server Alpha chat']);
    await act(async () => { bridge.emit('telemetry', { system: { cpu_percent: 12 }, workers: [], managed_totals: {} }); });
    assert.deepEqual(rows(), ['Server Alpha chat']);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Chat' }).props.onClick(); });
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Model server' }).props.onClick(); await flush(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Search local servers' }).props.value, 'SMALL');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Show selected models only' }).props.checked, true);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Server logs profile' }).props.value, 'p2');
    assert.deepEqual(rows(), ['Server Alpha chat']);
    assert.deepEqual(bridge.data.profiles, before);
    assert.equal(bridge.calls.some(call => ['profiles.save', 'server.start', 'server.stop', 'config.set'].includes(call.method)), false);
    const start = renderer.root.findByProps({ 'aria-label': 'Start selected' });
    assert.equal(start.props.disabled, false, 'the selected stopped profile hidden by search remains part of global start');
    await act(async () => { start.props.onClick(); await flush(); });
    assert.deepEqual(bridge.calls.filter(call => call.method === 'server.start').map(call => call.params), [{ all: true }]);
    assert.equal(bridge.storage.get('studio.server.search'), 'SMALL');
  } finally { await act(async () => renderer.unmount()); }
});

test('Server rows show observed activity, measured throughput and separate active and total request counts', async () => {
  harness();
  const data = snapshot(); data.servers[0].pid = 123; data.servers[0].active_requests = 2;
  const activity = { phase: 'preparing', phases: { preparing: 2 }, active_requests: 2, total_requests: 17, stats_available: true, tokens_per_second: null };
  const worker = { profile_id: 'p1', pid: 123, activity, stats: { requests: 17, engines: { locals: { completion_tokens: 1000, decode_s: 2 } } } };
  data.telemetry.workers = [worker];
  let renderer: TestRenderer.ReactTestRenderer;
  const render = async () => act(async () => { renderer.update(<ServerView snapshot={structuredClone(data)} onAction={async () => ({})} onLibrary={() => {}} />); await flush(); });
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={data} onAction={async () => ({})} onLibrary={() => {}} />); await flush(); });
  try {
    const reading = () => renderer.root.findByProps({ 'aria-label': 'Activity for test' });
    const requests = () => renderer.root.findByProps({ 'aria-label': 'Requests for test' });
    assert.match(renderedText(reading()), /Prompt.*— tok\/s/);
    assert.match(reading().findByProps({ className: 'server-activity-phase' }).props.title, /Queued.*first output/);
    assert.match(renderedText(requests()), /2 active.*17 total/);
    for (const [phase, label] of [['thinking', 'Thinking'], ['generating', 'Generating'], ['decoding', 'Decoding']]) {
      activity.phase = phase; activity.tokens_per_second = 42.25;
      await render();
      assert.equal(reading().props['data-phase'], phase);
      assert.match(renderedText(reading()), new RegExp(`${label}.*42\\.3 tok/s`));
    }
    activity.phase = 'mixed'; activity.phases = { thinking: 1, generating: 1 }; await render();
    assert.match(renderedText(reading()), /Mixed activity.*1 thinking.*1 generating.*42\.3 tok\/s/);
    activity.phase = 'idle'; activity.active_requests = 0; await render();
    assert.match(renderedText(reading()), /Idle.*— tok\/s/);
    assert.match(renderedText(requests()), /0 active.*17 total/);
    activity.phase = 'unknown'; activity.tokens_per_second = 500; activity.active_requests = 2; await render();
    assert.match(renderedText(reading()), /Unknown.*— tok\/s/, 'lifetime decode counters never become current throughput');
    activity.phase = 'generating'; activity.tokens_per_second = Number.NaN; await render();
    assert.match(renderedText(reading()), /Generating.*— tok\/s/);
    activity.tokens_per_second = 42; activity.stats_available = false; await render();
    assert.match(renderedText(reading()), /Generating.*— tok\/s/, 'an unavailable worker sample has no current throughput');
    activity.total_requests = null; await render();
    assert.match(renderedText(requests()), /2 active.*17 total/, 'an immediate phase event retains the current worker request total from its last statistics sample');
  } finally { await act(async () => renderer.unmount()); }
});

test('Server activity ignores stale worker PIDs and does not present stopped or loading workers as generating', async () => {
  harness();
  const data = snapshot(); data.servers[0].pid = 123; data.servers[0].active_requests = 0;
  const worker = { profile_id: 'p1', pid: 122, activity: { phase: 'generating', phases: { generating: 1 }, active_requests: 1, total_requests: 99, stats_available: true, tokens_per_second: 88 } };
  data.telemetry.workers = [worker];
  let renderer: TestRenderer.ReactTestRenderer;
  const render = async () => act(async () => { renderer.update(<ServerView snapshot={structuredClone(data)} onAction={async () => ({})} onLibrary={() => {}} />); await flush(); });
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={data} onAction={async () => ({})} onLibrary={() => {}} />); await flush(); });
  try {
    const reading = () => renderer.root.findByProps({ 'aria-label': 'Activity for test' });
    const requests = () => renderer.root.findByProps({ 'aria-label': 'Requests for test' });
    assert.match(renderedText(reading()), /Unknown.*— tok\/s/);
    assert.match(renderedText(requests()), /0 active.*— total/);
    worker.pid = 123;
    data.servers[0].started_at = '2026-10-26T12:00:00Z'; worker.started_at = '2026-10-25T12:00:00Z'; await render();
    assert.match(renderedText(reading()), /Unknown.*— tok\/s/, 'a reused PID with another start time is not the same worker');
    worker.started_at = data.servers[0].started_at; data.servers[0].port = 60777; worker.port = 60778; await render();
    assert.match(renderedText(reading()), /Unknown.*— tok\/s/, 'another private worker port is not the current process identity');
    worker.port = 60777; await render();
    assert.match(renderedText(reading()), /Generating.*88\.0 tok\/s/);
    data.servers[0].state = 'loading'; await render();
    assert.match(renderedText(reading()), /Loading model.*— tok\/s/);
    data.servers[0].state = 'stopped'; await render();
    assert.match(renderedText(reading()), /— tok\/s/);
    assert.equal((renderedText(renderer.root.findByProps({ 'aria-label': 'Server test' })).match(/stopped/gi) ?? []).length, 1, 'the stopped status appears once in the row');
    assert.match(renderedText(requests()), /0 active.*— total/);
    data.servers[0].state = 'failed'; await render();
    assert.match(renderedText(reading()), /Not running.*— tok\/s/);
  } finally { await act(async () => renderer.unmount()); }
});


test('immediate activity events survive older snapshots, telemetry and event deliveries without crossing worker identities', async () => {
  const pending = deferred<any>();
  let delayed = false;
  const bridge = harness({ snapshot: () => delayed ? pending.promise : structuredClone(bridge.data) });
  const timers = controlledIntervals();
  const started = '2026-10-26T12:00:00Z';
  const server = { ...bridge.data.servers[0], pid: 123, port: 60777, started_at: started };
  bridge.data.servers = [server];
  const reading = (phase: string, revision: number, patch: any = {}) => ({ phase, phases: { [phase]: phase === 'idle' ? 0 : 1 }, active_requests: phase === 'idle' ? 0 : 1, total_requests: 7, tokens_per_second: null, stats_available: false, tracker_epoch: 'epoch-a', revision, observed_at_ms: 100 + revision, ...patch });
  const worker = (activity: any, patch: any = {}) => ({ profile_id: 'p1', pid: 123, port: 60777, started_at: started, activity, ...patch });
  bridge.data.telemetry.workers = [worker(reading('idle', 0))];
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Model server' }).props.onClick(); await flush(); });
    const activity = () => renderer.root.findByProps({ 'aria-label': 'Activity for test' });
    assert.equal(activity().props['data-phase'], 'idle');
    delayed = true;
    await act(async () => { timers.tick(); await flush(); });
    await act(async () => { bridge.emit('server.activity', worker(reading('thinking', 2))); });
    assert.equal(activity().props['data-phase'], 'thinking', 'phase events do not wait for the next telemetry sample');
    await act(async () => { pending.resolve(structuredClone(bridge.data)); await flush(); });
    assert.equal(activity().props['data-phase'], 'thinking', 'an older full snapshot cannot restore idle');
    await act(async () => { bridge.emit('telemetry', { system: { cpu_percent: 15 }, workers: [worker(reading('preparing', 1))], managed_totals: {} }); });
    assert.equal(activity().props['data-phase'], 'thinking', 'older sampled activity cannot replace the immediate phase');
    await act(async () => { bridge.emit('server.activity', worker(reading('generating', 3))); });
    assert.equal(activity().props['data-phase'], 'generating');
    await act(async () => { bridge.emit('server.activity', worker(reading('preparing', 1))); });
    assert.equal(activity().props['data-phase'], 'generating', 'out-of-order transitions are ignored');
    await act(async () => { bridge.emit('telemetry', { system: {}, workers: [worker(reading('generating', 3, { stats_available: true, tokens_per_second: 55.5 }))], managed_totals: {} }); });
    assert.match(renderedText(activity()), /55\.5 tok\/s/, 'equal-revision fresh telemetry can supply its measured rate');
    await act(async () => { bridge.emit('server.activity', worker(reading('idle', 0, { tracker_epoch: 'epoch-b', observed_at_ms: 500 }))); });
    assert.equal(activity().props['data-phase'], 'idle', 'a new management-service epoch can restart revisions');
    await act(async () => { bridge.emit('telemetry', { system: {}, workers: [worker(reading('thinking', 20, { observed_at_ms: 400 }))], managed_totals: {} }); });
    assert.equal(activity().props['data-phase'], 'idle', 'a delayed response from the prior service cannot undo the new epoch');
    for (const wrongIdentity of [{ pid: 122 }, { port: 60778 }, { started_at: '2026-10-25T12:00:00Z' }]) {
      await act(async () => { bridge.emit('server.activity', worker(reading('thinking', 30, { tracker_epoch: 'epoch-b' }), wrongIdentity)); });
      assert.equal(activity().props['data-phase'], 'idle', 'only the current worker identity can update activity');
    }
    await act(async () => { bridge.emit('telemetry', { system: {}, workers: [], managed_totals: {} }); });
    assert.equal(activity().props['data-phase'], 'idle', 'an older empty telemetry list cannot erase a current worker phase');
    const nextServer = { ...server, pid: 456, port: 60779, started_at: '2026-10-26T13:00:00Z' };
    await act(async () => { bridge.emit('server.state', nextServer); });
    assert.equal(activity().props['data-phase'], 'unknown', 'a restarted process never inherits the old process activity');
    await act(async () => { bridge.emit('server.activity', { ...nextServer, activity: reading('thinking', 0, { tracker_epoch: 'epoch-b', observed_at_ms: 600 }) }); });
    assert.equal(activity().props['data-phase'], 'thinking');
  } finally { pending.resolve(bridge.data); await act(async () => renderer.unmount()); }
});


test('server logs start closed without log RPCs and all opening and closing paths persist across navigation and remounts', async () => {
  const bridge = harness();
  bridge.data.profiles.push({ ...profile, id: 'p2', alias: 'zzz-other', selected: false });
  const timers = controlledIntervals();
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  const navigate = async (label: string) => act(async () => { renderer.root.findByProps({ 'aria-label': label }).props.onClick(); await flush(); });
  const logReads = () => bridge.calls.filter(call => call.method === 'server.logs').length;
  try {
    await navigate('Model server');
    assert.equal(renderer.root.findAllByProps({ role: 'log', 'aria-label': 'Server logs' }).length, 0);
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Copy matching server logs' }).length, 0);
    await act(async () => { timers.tick(); await flush(); });
    assert.equal(logReads(), 0, 'a closed panel does not fetch or poll logs');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Expand logs' }).props.onClick(); await flush(); });
    assert.equal(logReads(), 1);
    assert.equal(bridge.storage.get('studio.server.logsOpen'), 'true');
    await navigate('Chat'); await navigate('Model server');
    assert.equal(renderer.root.findAllByProps({ role: 'log', 'aria-label': 'Server logs' }).length, 1, 'the open preference survives navigation');
    await act(async () => { renderer.root.findByProps({ className: 'logs-toggle' }).props.onClick(); });
    assert.equal(bridge.storage.get('studio.server.logsOpen'), 'false');
    const closedReads = logReads();
    await act(async () => { timers.tick(); await flush(); });
    assert.equal(logReads(), closedReads);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Logs for test' }).props.onClick(); await flush(); });
    assert.equal(bridge.storage.get('studio.server.logsOpen'), 'true', 'the explicit row log action saves the open preference');
    const row = (alias: string) => renderer.root.findAllByProps({ className: 'server-name' }).find(node => renderedText(node).includes(alias))!;
    await act(async () => { row('zzz-other').props.onClick(); await flush(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Server logs profile' }).props.value, 'p2');
    assert.equal(bridge.storage.get('studio.server.logsOpen'), 'true', 'selecting another row preserves an already open panel');
    assert.equal(renderer.root.findAllByProps({ role: 'log', 'aria-label': 'Server logs' }).length, 1);
    assert.equal(bridge.calls.filter(call => call.method === 'server.logs').at(-1)!.params.id, 'p2', 'an open panel follows the newly inspected profile');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Collapse logs' }).props.onClick(); });
    assert.equal(bridge.storage.get('studio.server.logsOpen'), 'false');
    const beforeSelection = logReads();
    await act(async () => { row('test').props.onClick(); await flush(); timers.tick(); await flush(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Server logs profile' }).props.value, 'p1');
    assert.equal(bridge.storage.get('studio.server.logsOpen'), 'false', 'selecting a row preserves a closed panel and its saved preference');
    assert.equal(renderer.root.findAllByProps({ role: 'log', 'aria-label': 'Server logs' }).length, 0);
    assert.equal(logReads(), beforeSelection, 'selecting a row while closed does not fetch or poll logs');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Logs for test' }).props.onClick(); await flush(); });
    assert.equal(bridge.storage.get('studio.server.logsOpen'), 'true', 'the explicit log action can open the panel again before app remount');
    await act(async () => { renderer.unmount(); renderer = TestRenderer.create(<App />); await flush(); });
    await navigate('Model server');
    assert.equal(renderer.root.findAllByProps({ role: 'log', 'aria-label': 'Server logs' }).length, 1, 'the preference survives a new app tree using saved local storage');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Collapse logs' }).props.onClick(); });
    await navigate('Chat'); await navigate('Model server');
    assert.equal(renderer.root.findAllByProps({ role: 'log', 'aria-label': 'Server logs' }).length, 0, 'a saved closed state survives navigation too');
  } finally { await act(async () => renderer.unmount()); }
});

test('log search preserves source line numbers, timestamps, severity and notices and copies only matching output', async () => {
  const timestamp = '2026-10-26T11:22:23.123Z';
  let failure = false;
  const payload = { text: 'structured entries take precedence', truncated: true, entries: [
    { line: 'Worker started', timestamp: '2026-10-26T11:22:22.000Z' },
    { line: 'Legacy ERROR from previous run', timestamp: null },
    { line: 'Error: failed request', timestamp },
    { line: '', timestamp: '2026-10-26T11:22:24.000Z' },
    { line: 'Ready', timestamp: '2026-10-26T11:22:25.000Z' },
  ] };
  const bridge = harness({ 'server.logs': params => {
    if (failure) throw new Error('Log file unavailable');
    return params.id === 'p2' ? { text: '', truncated: false, entries: [{ line: 'Other profile ERROR', timestamp: null }] } : structuredClone(payload);
  } });
  const timers = controlledIntervals();
  const copied: string[] = [];
  const oldNavigator = Object.getOwnPropertyDescriptor(globalThis, 'navigator');
  Object.defineProperty(globalThis, 'navigator', { configurable: true, value: { clipboard: { async writeText(value: string) { copied.push(value); } } } });
  window.setTimeout = (() => 0) as typeof window.setTimeout;
  const data = snapshot(); data.profiles.push({ ...profile, id: 'p2', alias: 'zzz-other' });
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={data} onAction={async () => ({})} onLibrary={() => {}} />); await flush(); });
  const rows = () => renderer.root.findAllByProps({ className: 'log-message' }).map(node => node.parent!);
  const search = async (value: string) => act(async () => { renderer.root.findByProps({ 'aria-label': 'Search server logs' }).props.onChange({ target: { value } }); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Expand logs' }).props.onClick(); await flush(); });
    assert.deepEqual(rows().map(row => row.props['data-line-number']), [1, 2, 3, 4, 5]);
    assert.doesNotMatch(renderedText(renderer.root.findByProps({ role: 'log', 'aria-label': 'Server logs' })), /structured entries take precedence/);
    const reads = bridge.calls.filter(call => call.method === 'server.logs').length;
    await search('eRrOr');
    assert.deepEqual(rows().map(row => row.props['data-line-number']), [2, 3], 'filtering retains original source positions');
    assert.ok(rows().every(row => row.props.className.split(' ').includes('error')));
    assert.equal(rows()[0].findAllByType('time').length, 0, 'legacy output does not gain an invented timestamp');
    assert.match(rows()[0].findByProps({ className: 'log-timestamp unknown' }).props.title, /not recorded/);
    assert.equal(rows()[1].findByType('time').props.dateTime, timestamp);
    assert.equal(renderedText(rows()[1].findByType('time')), '11:22:23.123');
    assert.match(rows()[1].findByType('time').props.title, /2026-10-26.*UTC/);
    await act(async () => { await renderer.root.findByProps({ 'aria-label': 'Copy matching server logs' }).props.onClick(); });
    assert.deepEqual(copied, [`Legacy ERROR from previous run\n[${timestamp}] Error: failed request`]);
    assert.equal(bridge.calls.filter(call => call.method === 'server.logs').length, reads, 'searching does not refetch logs');
    await search('11:22:23.123'); assert.deepEqual(rows().map(row => row.props['data-line-number']), [3]);
    await search('2026-10-26T11:22:23'); assert.deepEqual(rows().map(row => row.props['data-line-number']), [3]);
    await search('ERROR');
    payload.entries.push({ line: 'Error recovered', timestamp: '2026-10-26T11:22:26.000Z' });
    await act(async () => { timers.tick(); await flush(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Search server logs' }).props.value, 'ERROR');
    assert.deepEqual(rows().map(row => row.props['data-line-number']), [2, 3, 6]);
    await search('does not match anything');
    assert.equal(rows().length, 0);
    assert.match(renderedText(renderer.root.findByProps({ role: 'log', 'aria-label': 'Server logs' })), /No log lines match/);
    assert.match(renderedText(renderer.root.findByProps({ className: 'log-notices' })), /Earlier log lines omitted/);
    const copy = renderer.root.findByProps({ 'aria-label': 'Copy matching server logs' });
    assert.equal(copy.props.disabled, true);
    await act(async () => { await copy.props.onClick(); });
    assert.equal(copied.length, 1, 'an empty result does not replace the clipboard');
    failure = true;
    await act(async () => { timers.tick(); await flush(); });
    assert.match(renderedText(renderer.root.findByProps({ className: 'log-notices' })), /Log file unavailable/);
    assert.match(renderedText(renderer.root.findByProps({ className: 'log-notices' })), /Earlier log lines omitted/, 'notices remain visible regardless of search');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Clear log search' }).props.onClick(); });
    assert.equal(rows().length, 6, 'a failed refresh keeps the last successful output');
    failure = false;
    await search('ERROR');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Server logs profile' }).props.onChange({ target: { value: 'p2' } }); await flush(); });
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Search server logs' }).props.value, 'ERROR');
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Server logs profile' }).props.value, 'p2');
    assert.deepEqual(rows().map(row => row.props['data-line-number']), [1]);
    assert.match(renderedText(rows()[0]), /Other profile ERROR/);
    assert.equal(renderer.root.findAllByProps({ className: 'log-notices' }).length, 0);
  } finally {
    await act(async () => renderer.unmount());
    if (oldNavigator) Object.defineProperty(globalThis, 'navigator', oldNavigator); else delete (globalThis as any).navigator;
  }
});

test('server logs follow the latest output in both orders and preserve a paused viewport until Follow resumes', async () => {
  const timestamp = '2026-10-27T09:00:00.000Z';
  const payload = { truncated: false, entries: [{ line: 'First', timestamp }, { line: 'Second', timestamp: null as string | null }] };
  const bridge = harness({ 'server.logs': () => structuredClone(payload) });
  const timers = controlledIntervals();
  const viewport = mockLogViewport();
  const copied: string[] = [];
  const oldNavigator = Object.getOwnPropertyDescriptor(globalThis, 'navigator');
  Object.defineProperty(globalThis, 'navigator', { configurable: true, value: { clipboard: { async writeText(value: string) { copied.push(value); } } } });
  window.setTimeout = (() => 0) as typeof window.setTimeout;
  const createNodeMock = (element: any) => element.props.role === 'log' && element.props['aria-label'] === 'Server logs' ? viewport : null;
  const content = <ServerView snapshot={snapshot()} onAction={async () => ({})} onLibrary={() => {}} />;
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(content, { createNodeMock }); await flush(); });
  const log = () => renderer.root.findByProps({ role: 'log', 'aria-label': 'Server logs' });
  const follow = () => renderer.root.findByProps({ 'aria-label': 'Follow logs' });
  const order = () => renderer.root.findByProps({ 'aria-label': 'Server log order' });
  const numbers = () => log().findAllByProps({ className: 'log-message' }).map(node => node.parent!.props['data-line-number']);
  const scroll = async (top: number) => act(async () => { viewport.scrollTop = top; log().props.onScroll({ currentTarget: viewport, target: viewport }); });
  const append = async (line: string, height: number) => act(async () => { payload.entries.push({ line, timestamp: null }); viewport.scrollHeight = height; timers.tick(); await flush(); });
  try {
    assert.equal(renderer.root.findAllByProps({ role: 'log', 'aria-label': 'Server logs' }).length, 0);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Expand logs' }).props.onClick(); await flush(); });
    assert.equal(order().props.value, 'asc', 'the default order reads oldest to newest');
    assert.deepEqual(numbers(), [1, 2]);
    assert.equal(viewport.scrollTop, 800, 'opening starts at the latest output at the bottom');
    assert.equal(follow().props['aria-pressed'], true);
    await append('Third', 1200);
    assert.equal(viewport.scrollTop, 1000, 'new ascending output remains visible while following');
    await scroll(350);
    assert.equal(follow().props['aria-pressed'], false, 'scrolling away pauses automatic following');
    await append('Fourth', 1400);
    assert.equal(viewport.scrollTop, 350, 'appended ascending output does not move a paused reader');
    assert.equal(follow().props['aria-pressed'], false);
    await act(async () => { follow().props.onClick(); });
    assert.equal(follow().props['aria-pressed'], true);
    assert.equal(viewport.scrollTop, 1200, 'Follow returns to the current bottom immediately');

    await act(async () => { order().props.onChange({ target: { value: 'desc' } }); });
    assert.equal(bridge.storage.get('studio.server.logOrder'), 'desc');
    assert.deepEqual(numbers(), [4, 3, 2, 1], 'reversing retains original source line numbers');
    assert.equal(viewport.scrollTop, 0, 'the latest output is at the top in descending order');
    assert.equal(follow().props['aria-pressed'], true);
    await append('Fifth', 1600);
    assert.equal(viewport.scrollTop, 0, 'new descending output remains visible while following');
    assert.deepEqual(numbers(), [5, 4, 3, 2, 1]);
    await scroll(250);
    assert.equal(follow().props['aria-pressed'], false);
    await append('Sixth', 1800);
    assert.equal(viewport.scrollTop, 450, 'a paused descending reader keeps the same output after new rows are prepended');
    assert.equal(follow().props['aria-pressed'], false);
    await act(async () => { await renderer.root.findByProps({ 'aria-label': 'Copy matching server logs' }).props.onClick(); });
    assert.equal(copied.at(-1), `Sixth\nFifth\nFourth\nThird\nSecond\n[${timestamp}] First`, 'clipboard order matches the displayed descending order');
    await act(async () => { follow().props.onClick(); });
    assert.equal(viewport.scrollTop, 0);
    assert.equal(follow().props['aria-pressed'], true);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Collapse logs' }).props.onClick(); });
    viewport.scrollTop = 500;
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Expand logs' }).props.onClick(); await flush(); });
    assert.equal(viewport.scrollTop, 0, 'reopening returns to the latest output');
    await act(async () => { renderer.unmount(); viewport.scrollTop = 500; renderer = TestRenderer.create(content, { createNodeMock }); await flush(); });
    assert.equal(order().props.value, 'desc', 'the selected order survives a fresh component tree');
    assert.deepEqual(numbers(), [6, 5, 4, 3, 2, 1]);
    assert.equal(viewport.scrollTop, 0);
    assert.equal(follow().props['aria-pressed'], true);
  } finally {
    await act(async () => renderer.unmount());
    if (oldNavigator) Object.defineProperty(globalThis, 'navigator', oldNavigator); else delete (globalThis as any).navigator;
  }
});

test('log search, profile changes, order changes and navigation resume following without losing the saved order', async () => {
  const bridge = harness({ 'server.logs': ({ id }) => ({ truncated: false, entries: [{ line: `${id} kept first`, timestamp: null }, { line: `${id} latest`, timestamp: null }] }) });
  bridge.data.profiles.push({ ...profile, id: 'p2', alias: 'zzz-other' });
  bridge.storage.set('studio.server.logOrder', 'invalid-old-value');
  controlledIntervals();
  const viewport = mockLogViewport();
  const createNodeMock = (element: any) => element.props.role === 'log' && element.props['aria-label'] === 'Server logs' ? viewport : null;
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />, { createNodeMock }); await flush(); });
  const log = () => renderer.root.findByProps({ role: 'log', 'aria-label': 'Server logs' });
  const follow = () => renderer.root.findByProps({ 'aria-label': 'Follow logs' });
  const order = () => renderer.root.findByProps({ 'aria-label': 'Server log order' });
  const scroll = async (top: number) => act(async () => { viewport.scrollTop = top; log().props.onScroll({ currentTarget: viewport, target: viewport }); });
  const navigate = async (name: string) => act(async () => { renderer.root.findByProps({ 'aria-label': name }).props.onClick(); await flush(); });
  try {
    await navigate('Model server');
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Expand logs' }).props.onClick(); await flush(); });
    assert.equal(order().props.value, 'asc', 'an unknown persisted value falls back to oldest first');
    assert.equal(viewport.scrollTop, 800);
    await act(async () => { order().props.onChange({ target: { value: 'desc' } }); });
    await scroll(250);
    const reads = bridge.calls.filter(call => call.method === 'server.logs').length;
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Search server logs' }).props.onChange({ target: { value: 'kept' } }); });
    assert.equal(follow().props['aria-pressed'], true, 'a new search starts following its latest match');
    assert.equal(viewport.scrollTop, 0);
    assert.equal(bridge.calls.filter(call => call.method === 'server.logs').length, reads, 'filtering existing output needs no additional read');
    await scroll(250);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Server logs profile' }).props.onChange({ target: { value: 'p2' } }); await flush(); });
    assert.equal(follow().props['aria-pressed'], true, 'another server starts at its latest output');
    assert.equal(viewport.scrollTop, 0);
    assert.match(renderedText(log()), /p2 kept first/);
    assert.equal(order().props.value, 'desc');
    await scroll(250);
    await act(async () => { order().props.onChange({ target: { value: 'asc' } }); });
    assert.equal(viewport.scrollTop, 800, 'changing order resumes following at the matching edge');
    assert.equal(follow().props['aria-pressed'], true);
    assert.equal(bridge.storage.get('studio.server.logOrder'), 'asc');
    await scroll(250);
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Clear log search' }).props.onClick(); });
    assert.equal(viewport.scrollTop, 800, 'clearing search also returns to latest');
    assert.equal(follow().props['aria-pressed'], true);
    await scroll(250);
    await navigate('Chat');
    viewport.scrollTop = 100;
    await navigate('Model server');
    assert.equal(order().props.value, 'asc', 'the saved order survives navigation');
    assert.equal(follow().props['aria-pressed'], true, 'reopening the saved panel follows again');
    assert.equal(viewport.scrollTop, 800);
  } finally { await act(async () => renderer.unmount()); }
});

test('a paused log reader keeps the same visible line when the bounded tail rotates without changing height', async () => {
  const payload = { truncated: true, entries: ['A', 'B', 'C', 'D', 'E', 'F'].map(line => ({ line, timestamp: null })) };
  harness({ 'server.logs': () => structuredClone(payload) });
  const timers = controlledIntervals();
  const viewport = mockLogViewport();
  viewport.clientHeight = 60;
  viewport.scrollHeight = 120;
  let renderer: TestRenderer.ReactTestRenderer | undefined;
  const domRows = () => (renderer?.root.findAll(node => typeof node.props['data-log-key'] === 'string') ?? []).map((row, index) => ({
    dataset: { logKey: row.props['data-log-key'] },
    getAttribute: (name: string) => name === 'data-log-key' ? row.props['data-log-key'] : null,
    getBoundingClientRect: () => ({ top: index * 20 - viewport.scrollTop, bottom: (index + 1) * 20 - viewport.scrollTop, height: 20 }),
  }));
  const element = Object.assign(viewport, {
    getBoundingClientRect: () => ({ top: 0, bottom: viewport.clientHeight, height: viewport.clientHeight }),
    querySelectorAll: () => domRows(),
  });
  const createNodeMock = (node: any) => node.props.role === 'log' && node.props['aria-label'] === 'Server logs' ? element : null;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={snapshot()} onAction={async () => ({})} onLibrary={() => {}} />, { createNodeMock }); await flush(); });
  const log = () => renderer!.root.findByProps({ role: 'log', 'aria-label': 'Server logs' });
  const follow = () => renderer!.root.findByProps({ 'aria-label': 'Follow logs' });
  const scroll = async (top: number) => act(async () => { viewport.scrollTop = top; log().props.onScroll({ currentTarget: element, target: element }); });
  const rotate = async (line: string) => act(async () => { payload.entries.shift(); payload.entries.push({ line, timestamp: null }); timers.tick(); await flush(); });
  try {
    await act(async () => { renderer!.root.findByProps({ 'aria-label': 'Expand logs' }).props.onClick(); await flush(); });
    await scroll(40);
    assert.equal(follow().props['aria-pressed'], false);
    await rotate('G');
    assert.equal(viewport.scrollTop, 20, 'ascending rotation keeps C at the top after A leaves the bounded tail');
    assert.equal(follow().props['aria-pressed'], false);
    await act(async () => { renderer!.root.findByProps({ 'aria-label': 'Server log order' }).props.onChange({ target: { value: 'desc' } }); });
    await scroll(40);
    assert.equal(follow().props['aria-pressed'], false);
    await rotate('H');
    assert.equal(viewport.scrollTop, 60, 'descending rotation keeps E at the top after H arrives');
    assert.equal(follow().props['aria-pressed'], false);
  } finally { await act(async () => renderer!.unmount()); }
});

test('identical descending log polls keep a paused reader on the same repeated legacy lines', async () => {
  const payload = { truncated: false, entries: Array.from({ length: 20 }, () => ({ line: 'repeated', timestamp: null })) };
  const bridge = harness({ 'server.logs': () => structuredClone(payload) });
  bridge.storage.set('studio.server.logOrder', 'desc');
  const timers = controlledIntervals();
  const viewport = mockLogViewport();
  viewport.clientHeight = 60;
  viewport.scrollHeight = 400;
  let renderer: TestRenderer.ReactTestRenderer | undefined;
  const element = Object.assign(viewport, {
    getBoundingClientRect: () => ({ top: 0, bottom: viewport.clientHeight, height: viewport.clientHeight }),
    querySelectorAll: () => (renderer?.root.findAll(node => typeof node.props['data-log-key'] === 'string') ?? []).map((row, index) => ({
      dataset: { logKey: row.props['data-log-key'] },
      getBoundingClientRect: () => ({ top: index * 20 - viewport.scrollTop, bottom: (index + 1) * 20 - viewport.scrollTop, height: 20 }),
    })),
  });
  const createNodeMock = (node: any) => node.props.role === 'log' && node.props['aria-label'] === 'Server logs' ? element : null;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={snapshot()} onAction={async () => ({})} onLibrary={() => {}} />, { createNodeMock }); await flush(); });
  try {
    await act(async () => { renderer!.root.findByProps({ 'aria-label': 'Expand logs' }).props.onClick(); await flush(); });
    const log = () => renderer!.root.findByProps({ role: 'log', 'aria-label': 'Server logs' });
    const follow = () => renderer!.root.findByProps({ 'aria-label': 'Follow logs' });
    await act(async () => { viewport.scrollTop = 180; log().props.onScroll({ currentTarget: element, target: element }); });
    assert.equal(follow().props['aria-pressed'], false);
    for (let poll = 0; poll < 3; poll += 1) {
      await act(async () => { timers.tick(); await flush(); });
      assert.equal(viewport.scrollTop, 180, 'an unchanged repeated line must not anchor to its first occurrence');
      assert.equal(follow().props['aria-pressed'], false);
    }
  } finally { await act(async () => renderer!.unmount()); }
});

test('delayed programmatic log scroll events respect Pause and manually reaching latest resumes Follow', async () => {
  const payload = { truncated: false, entries: [{ line: 'Initial output', timestamp: null }] };
  harness({ 'server.logs': () => structuredClone(payload) });
  const timers = controlledIntervals();
  const viewport = mockLogViewport();
  const createNodeMock = (node: any) => node.props.role === 'log' && node.props['aria-label'] === 'Server logs' ? viewport : null;
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<ServerView snapshot={snapshot()} onAction={async () => ({})} onLibrary={() => {}} />, { createNodeMock }); await flush(); });
  const log = () => renderer.root.findByProps({ role: 'log', 'aria-label': 'Server logs' });
  const follow = () => renderer.root.findByProps({ 'aria-label': 'Follow logs' });
  const emitScroll = async () => act(async () => { log().props.onScroll({ currentTarget: viewport, target: viewport }); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Expand logs' }).props.onClick(); await flush(); });
    assert.equal(viewport.scrollTop, 800);
    await act(async () => { follow().props.onClick(); });
    assert.equal(follow().props['aria-pressed'], false);
    await emitScroll();
    assert.equal(follow().props['aria-pressed'], false, 'the delayed automatic bottom scroll must not undo a subsequent Pause');
    await act(async () => { payload.entries.push({ line: 'More output', timestamp: null }); viewport.scrollHeight = 1200; timers.tick(); await flush(); });
    assert.equal(viewport.scrollTop, 800, 'Pause still applies to the next append');
    await act(async () => { viewport.scrollTop = 400; }); await emitScroll();
    await act(async () => { viewport.scrollTop = 1000; }); await emitScroll();
    assert.equal(follow().props['aria-pressed'], true, 'manually returning to the latest bottom resumes following');

    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Server log order' }).props.onChange({ target: { value: 'desc' } }); });
    assert.equal(viewport.scrollTop, 0);
    await act(async () => { follow().props.onClick(); });
    await emitScroll();
    assert.equal(follow().props['aria-pressed'], false, 'the delayed automatic top scroll must not undo Pause either');
    await act(async () => { payload.entries.push({ line: 'Newest output', timestamp: null }); viewport.scrollHeight = 1400; timers.tick(); await flush(); });
    assert.equal(viewport.scrollTop, 200, 'paused descending output is anchored after a prepend');
    await act(async () => { viewport.scrollTop = 0; }); await emitScroll();
    assert.equal(follow().props['aria-pressed'], true, 'manually returning to the latest top resumes following');
  } finally { await act(async () => renderer.unmount()); }
});


test('a new GUI identifies an old busy service and resumes live activity after its automatic capability upgrade', async () => {
  const current = () => structuredClone(data);
  const bridge = harness({ snapshot: () => current() });
  const timers = controlledIntervals();
  const data = snapshot();
  data.servers[0] = { ...data.servers[0], pid: 123, port: 60777, started_at: '2026-10-26T12:00:00Z', active_requests: 1 };
  data.service = { service: 'locals-studio', version: '0.1.0', capabilities: [], upgrade: { status: 'pending', reason: 'An inference request is still active.' } };
  const legacyWorker = { profile_id: 'p1', pid: 123, port: 60777, started_at: data.servers[0].started_at, stats: { requests: 4, active: [{ engine: 'locals', prompt_tokens: 12, tokens: 30 }] } };
  data.telemetry.workers = [legacyWorker];
  const oldSnapshot = structuredClone(data);
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Model server' }).props.onClick(); await flush(); });
    const reading = () => renderer.root.findByProps({ 'aria-label': 'Activity for test' });
    const banner = () => renderer.root.findAllByProps({ 'aria-label': 'Studio service update' });
    assert.equal(banner().length, 1);
    assert.match(renderedText(banner()[0]), /update pending.*retry.*when it is idle/i);
    assert.match(renderedText(banner()[0]), /inference request is still active/);
    assert.equal(reading().props['data-phase'], 'service_update_pending');
    assert.match(renderedText(reading()), /Service update pending.*— tok\/s/);
    assert.equal(renderer.root.findAllByProps({ role: 'dialog' }).length, 0, 'the update does not add a confirmation flow');
    assert.equal(bridge.calls.some(call => ['server.stop', 'server.start'].includes(call.method)), false);

    data.service = { service: 'locals-studio', version: '0.1.0', capabilities: ['worker_activity_v1', 'worker_log_entries_v1'], upgrade: { status: 'current' } };
    data.telemetry.workers = [{ ...legacyWorker, activity: { phase: 'generating', phases: { generating: 1 }, active_requests: 1, total_requests: 4, stats_available: true, tokens_per_second: 60, tracker_epoch: 'new-service', revision: 2, observed_at_ms: 200 } }];
    await act(async () => { timers.tick(); await flush(); });
    assert.equal(banner().length, 0, 'the capability snapshot clears the update notice');
    assert.equal(reading().props['data-phase'], 'generating');
    assert.match(renderedText(reading()), /60\.0 tok\/s/);

    const upgraded = structuredClone(data);
    Object.assign(data, oldSnapshot);
    await act(async () => { timers.tick(); await flush(); });
    assert.equal(banner().length, 0, 'a stale legacy snapshot cannot remove observed current service capabilities');
    assert.equal(reading().props['data-phase'], 'generating', 'older telemetry without activity cannot erase the live phase');
    Object.assign(data, upgraded);
    data.telemetry.workers[0].activity = { ...data.telemetry.workers[0].activity, phase: 'unknown', phases: { unknown: 1 }, revision: 3, stats_available: false, tokens_per_second: null };
    await act(async () => { timers.tick(); await flush(); });
    assert.equal(banner().length, 0, 'an unavailable modern worker sample is not an old service');
    assert.equal(reading().props['data-phase'], 'unknown');
    assert.match(renderedText(reading()), /Unknown.*— tok\/s/);

    Object.assign(data, oldSnapshot);
    await act(async () => { bridge.emit('service.disconnected', { error: 'Service restarted' }); bridge.emit('connected', {}); await flush(); });
    assert.equal(banner().length, 1, 'a new connection checks capabilities again rather than retaining an old capability floor');
    assert.equal(reading().props['data-phase'], 'service_update_pending');
  } finally { await act(async () => renderer.unmount()); }
});

test('legacy worker telemetry without capability metadata is explained instead of showing an ambiguous Unknown phase', async () => {
  const bridge = harness();
  bridge.data.servers[0].pid = 123;
  bridge.data.telemetry.workers = [{ profile_id: 'p1', pid: 123, stats: { requests: 1, active: [] } }];
  let renderer: TestRenderer.ReactTestRenderer;
  await act(async () => { renderer = TestRenderer.create(<App />); await flush(); });
  try {
    await act(async () => { renderer.root.findByProps({ 'aria-label': 'Model server' }).props.onClick(); await flush(); });
    assert.equal(renderer.root.findAllByProps({ 'aria-label': 'Studio service update' }).length, 1);
    assert.equal(renderer.root.findByProps({ 'aria-label': 'Activity for test' }).props['data-phase'], 'service_update_pending');
  } finally { await act(async () => renderer.unmount()); }
});
