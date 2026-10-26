import { useCallback, useEffect, useRef, useState } from 'react';
import { AlertCircle, ArrowDownToLine, BookOpen, Check, Cpu, FolderOpen, MessageSquare, Moon, PanelLeftClose, Radio, RefreshCw, Search, Server as ServerIcon, Settings2, Sun, X } from 'lucide-react';
import { errorMessage, request, subscribe } from './api';
import { EMPTY_SNAPSHOT, type Conversation, type JsonObject, type Snapshot, type StudioEvent } from './types';
import { bytes, downloadState, mergeMessageDelta } from './utils.mjs';
import { Modal, Spinner } from './components';
import ChatView from './views/ChatView';
import ServerView from './views/ServerView';
import LibraryView from './views/LibraryView';
import SearchView from './views/SearchView';
import AboutPanel from './AboutPanel';
import BrandIcon from './BrandIcon';
import { applyAppearance, loadAppearance, normalizeAppearance, SettingsAppearance, type Appearance } from './appearance';

type View = 'chat' | 'server' | 'library' | 'search';
type SettingsTab = 'general' | 'about';
const navigation = [
  { id: 'chat' as const, icon: MessageSquare, name: 'Chat' },
  { id: 'server' as const, icon: ServerIcon, name: 'Model server' },
  { id: 'library' as const, icon: BookOpen, name: 'My models' },
  { id: 'search' as const, icon: Search, name: 'Discover' },
];

export default function App() {
  const [view, setView] = useState<View>('chat');
  const [snapshot, setSnapshot] = useState<Snapshot>(EMPTY_SNAPSHOT);
  const [connected, setConnected] = useState(false);
  const [eventConnection, setEventConnection] = useState<'connecting' | 'connected' | 'disconnected'>('connecting');
  const [eventError, setEventError] = useState('');
  const connectionEpoch = useRef(0);
  const reconnectEvents = useRef<() => void>(() => {});
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const [appearance, setAppearance] = useState<Appearance>(loadAppearance);
  const { theme } = appearance;
  const [selectedServerProfile, setSelectedServerProfile] = useState('');
  const [settings, setSettings] = useState(false);
  const [settingsTab, setSettingsTab] = useState<SettingsTab>('general');
  const [streams, setStreams] = useState<Record<string, string>>({});
  const [chatErrors, setChatErrors] = useState<Record<string, string>>({});
  const streamRef = useRef(streams);
  const completedRequests = useRef(new Set<string>());
  streamRef.current = streams;
  const refreshing = useRef(false);
  const folderMoveVersion = useRef(0);
  const folderMoves = useRef(new Map<string, { version: number; folder_id: string | null; updated_at?: string }>());
  const folderVersion = useRef(0);
  const folderDeletes = useRef(new Map<string, number>());
  const folderSaves = useRef(new Map<string, number>());
  const liveFolderIds = useRef<Set<string> | null>(null);
  const conversationDeleteVersion = useRef(0);
  const conversationDeletes = useRef(new Map<string, number>());
  useEffect(() => { applyAppearance(appearance); }, [appearance]);
  const updateAppearance = useCallback((patch: Partial<Appearance>) => setAppearance(current => normalizeAppearance({ ...current, ...patch })), []);
  const normalizeFolder = (conversation: Conversation, acknowledgedVersion = -1): Conversation => {
    const id = conversation.folder_id;
    if (typeof id === 'string' && ((folderDeletes.current.get(id) ?? -1) > acknowledgedVersion || (liveFolderIds.current !== null && !liveFolderIds.current.has(id)))) return { ...conversation, folder_id: null };
    return conversation;
  };

  const refresh = useCallback(async () => {
    if (refreshing.current) return;
    refreshing.current = true;
    const connectionAtStart = connectionEpoch.current;
    const movesAtStart = folderMoveVersion.current;
    const foldersAtStart = folderVersion.current;
    const deletesAtStart = conversationDeleteVersion.current;
    try {
      const value = await request<Partial<Snapshot>>('snapshot');
      // A read started before a disconnect must not restore a stale green status.
      if (connectionAtStart !== connectionEpoch.current) return;
      setSnapshot(previous => {
        const next = { ...EMPTY_SNAPSHOT, ...value };
        next.runtime = { ...previous.runtime, ...next.runtime };
        next.folders = (next.folders ?? []).filter(folder => (folderDeletes.current.get(folder.id) ?? 0) <= foldersAtStart);
        const folderIds = new Set(next.folders.map(folder => folder.id));
        // A successful create may finish while an older snapshot is still pending.
        // Keep that confirmed ID until a read begun after the save acknowledges it.
        for (const [id, version] of folderSaves.current) {
          if (version > foldersAtStart) folderIds.add(id);
        }
        liveFolderIds.current = folderIds;
        next.conversations = (next.conversations ?? []).filter(conversation => (conversationDeletes.current.get(conversation.id) ?? 0) <= deletesAtStart).map(conversation => {
          const moved = folderMoves.current.get(conversation.id);
          const incoming = normalizeFolder(moved && moved.version > movesAtStart ? { ...conversation, folder_id: moved.folder_id, updated_at: moved.updated_at } : conversation, foldersAtStart);
          const live = streamRef.current[conversation.id] && previous.conversations.find(old => old.id === conversation.id);
          return live ? { ...live, folder_id: incoming.folder_id ?? null, updated_at: incoming.updated_at ?? live.updated_at } : incoming;
        });
        for (const [id, version] of folderDeletes.current) {
          if (version <= foldersAtStart) folderDeletes.current.delete(id);
        }
        for (const [id, version] of folderSaves.current) {
          if (version <= foldersAtStart) folderSaves.current.delete(id);
        }
        for (const [id, version] of conversationDeletes.current) {
          if (version <= deletesAtStart) conversationDeletes.current.delete(id);
        }
        return next;
      });
      setConnected(true); setError('');
    } catch (failure) { if (connectionAtStart === connectionEpoch.current) { setConnected(false); setError(errorMessage(failure)); } }
    finally { setLoading(false); refreshing.current = false; }
  }, []);

  const handleEvent = useCallback((event: StudioEvent) => {
    if (event.type === 'service.disconnected') {
      connectionEpoch.current += 1;
      setEventConnection('disconnected');
      setEventError(String(event.data.error ?? 'The connection to Studio updates was interrupted.'));
      return;
    }
    if (event.type === 'connected' || event.type === 'service.connected') {
      setEventConnection('connected'); setEventError(''); void refresh(); return;
    }
    if (event.type === 'desktop.about') { setSettingsTab('about'); setSettings(true); return; }
    if (event.type === 'desktop.settings') { setSettingsTab('general'); setSettings(true); return; }
    if (event.type === 'telemetry') { setSnapshot(previous => ({ ...previous, telemetry: event.data })); return; }
    if (event.type === 'servers.changed') { if (Array.isArray(event.data)) setSnapshot(previous => ({ ...previous, servers: event.data as any[] })); return; }
    if (event.type === 'server.state') { setSnapshot(previous => ({ ...previous, servers: [...previous.servers.filter(server => server.profile_id !== event.data.profile_id), event.data as any] })); return; }
    if (event.type === 'download.updated') { setSnapshot(previous => ({ ...previous, downloads: [...previous.downloads.filter(download => download.id !== event.data.id), event.data as any] })); return; }
    if (event.type === 'models.updated') { setSnapshot(previous => ({ ...previous, models: [...previous.models.filter(model => model.id !== event.data.id), event.data as any] })); return; }
    if (event.type === 'api.changed') { setSnapshot(previous => ({ ...previous, api: event.data })); return; }
    if (event.type === 'runtime.progress' || event.type === 'runtime.error') { setSnapshot(previous => ({ ...previous, runtime: { ...previous.runtime, ...event.data } })); if (event.data.phase === 'ready') void refresh(); return; }
    if (event.type === 'runtime.log') { setSnapshot(previous => ({ ...previous, runtime: { ...previous.runtime, logs: [...(previous.runtime.logs ?? []), event.data.line].slice(-100) } })); return; }
    if (event.type === 'chat.delta') {
      const { conversation_id, delta, reasoning_delta, request_id } = event.data;
      const active = streamRef.current[conversation_id];
      if (!active || completedRequests.current.has(request_id) || !active.startsWith('starting:') && active !== request_id) return;
      setSnapshot(previous => ({ ...previous, conversations: previous.conversations.map(conversation => conversation.id === conversation_id ? { ...conversation, messages: mergeMessageDelta(conversation.messages ?? [], delta ?? '', reasoning_delta ?? '') } : conversation) }));
    } else if (event.type === 'chat.done' || event.type === 'chat.error') {
      const { conversation_id, request_id, message } = event.data;
      const active = streamRef.current[conversation_id];
      if (completedRequests.current.has(request_id) || active && !active.startsWith('starting:') && active !== request_id) return;
      completedRequests.current.add(request_id);
      if (completedRequests.current.size > 256) completedRequests.current.delete(completedRequests.current.values().next().value!);
      const nextStreams = { ...streamRef.current }; delete nextStreams[conversation_id]; streamRef.current = nextStreams;
      if (message) setSnapshot(previous => ({ ...previous, conversations: previous.conversations.map(conversation => conversation.id === conversation_id ? { ...conversation, messages: conversation.messages.at(-1)?.role === 'assistant' ? [...conversation.messages.slice(0, -1), message] : [...conversation.messages, message] } : conversation) }));
      if (message?.error) setChatErrors(previous => ({ ...previous, [conversation_id]: String(message.error) }));
      setStreams(previous => { const next = { ...previous }; delete next[conversation_id]; return next; });
      if (event.type === 'chat.error') setChatErrors(previous => ({ ...previous, [conversation_id]: String(event.data.error ?? 'Generation failed.') }));
      void refresh();
    } else { void refresh(); }
  }, [refresh]);

  useEffect(() => {
    let dispose: (() => void) | undefined;
    let mounted = true;
    let subscribing = false;
    const connectEvents = () => {
      if (!mounted || dispose || subscribing) return;
      subscribing = true;
      void subscribe(handleEvent).then(stop => { if (mounted) dispose = stop; else stop(); }).catch(failure => {
        if (mounted) { connectionEpoch.current += 1; setEventConnection('disconnected'); setEventError(errorMessage(failure)); }
      }).finally(() => { subscribing = false; });
    };
    reconnectEvents.current = connectEvents;
    void refresh(); connectEvents();
    const timer = window.setInterval(() => { if (!document.hidden) { connectEvents(); void refresh(); } }, 3000);
    return () => { mounted = false; reconnectEvents.current = () => {}; dispose?.(); window.clearInterval(timer); };
  }, [refresh, handleEvent]);

  const action = useCallback(async (method: string, params: JsonObject = {}) => {
    const foldersAtRequest = folderVersion.current;
    try {
      let result = await request(method, params);
      if (method === 'folders.save' && typeof result?.id === 'string' && (folderDeletes.current.get(result.id) ?? -1) <= foldersAtRequest) {
        folderSaves.current.set(result.id, ++folderVersion.current);
        folderDeletes.current.delete(result.id);
        liveFolderIds.current?.add(result.id);
      }
      if (method === 'folders.remove' && typeof params.id === 'string') {
        const id = params.id;
        folderDeletes.current.set(id, ++folderVersion.current);
        folderSaves.current.delete(id);
        liveFolderIds.current?.delete(id);
        setSnapshot(previous => ({ ...previous, folders: previous.folders.filter(folder => folder.id !== id), conversations: previous.conversations.map(conversation => conversation.folder_id === id ? { ...conversation, folder_id: null } : conversation) }));
      }
      if (method === 'conversations.move' && typeof result?.id === 'string') {
        result = normalizeFolder(result);
        const moved = { version: ++folderMoveVersion.current, folder_id: result.folder_id ?? null, updated_at: result.updated_at };
        folderMoves.current.set(result.id, moved);
        setSnapshot(previous => ({ ...previous, conversations: previous.conversations.map(conversation => conversation.id === result.id ? normalizeFolder({ ...conversation, folder_id: moved.folder_id, updated_at: moved.updated_at }) : conversation) }));
      }
      if (method === 'conversations.remove' && typeof params.id === 'string') {
        const id = params.id;
        conversationDeletes.current.set(id, ++conversationDeleteVersion.current);
        setSnapshot(previous => ({ ...previous, conversations: previous.conversations.filter(conversation => conversation.id !== id) }));
      }
      await refresh(); return result;
    }
    catch (failure) { setNotice(errorMessage(failure)); throw failure; }
  }, [refresh]);
  const run = (method: string, params: JsonObject = {}) => { void action(method, params).catch(() => {}); };

  const saveConversation = async (conversation: Conversation) => {
    if (conversationDeletes.current.has(conversation.id)) throw new Error('This conversation was deleted. Start a new conversation.');
    const saved = normalizeFolder(conversation);
    await action('conversations.save', { conversation: saved });
    setSnapshot(previous => ({ ...previous, conversations: [...previous.conversations.filter(item => item.id !== saved.id), normalizeFolder(saved)] }));
  };
  const send = async (conversation: Conversation, text: string, model: string, parameters: JsonObject) => {
    if (conversationDeletes.current.has(conversation.id)) { setNotice('This conversation was deleted. Start a new conversation.'); return; }
    if (streamRef.current[conversation.id]) return;
    const pendingId = `starting:${crypto.randomUUID()}`;
    const userMessage = { role: 'user', content: text };
    const pending = normalizeFolder({ ...conversation, title: conversation.messages.length ? conversation.title : text.slice(0, 52), model, messages: [...conversation.messages, userMessage] });
    setChatErrors(previous => ({ ...previous, [conversation.id]: '' }));
    streamRef.current = { ...streamRef.current, [conversation.id]: pendingId };
    setStreams(previous => ({ ...previous, [conversation.id]: pendingId }));
    setSnapshot(previous => ({ ...previous, conversations: [...previous.conversations.filter(item => item.id !== pending.id), pending] }));
    try {
      await request('conversations.save', { conversation: pending });
      const started = await request('chat.start', { conversation_id: conversation.id, model, messages: pending.messages, parameters });
      if (streamRef.current[conversation.id] === pendingId) streamRef.current = { ...streamRef.current, [conversation.id]: started.request_id ?? started.id };
      setStreams(previous => previous[conversation.id] === pendingId ? ({ ...previous, [conversation.id]: started.request_id ?? started.id }) : previous);
    } catch (failure) {
      if (streamRef.current[conversation.id] !== pendingId) return;
      const nextStreams = { ...streamRef.current }; delete nextStreams[conversation.id]; streamRef.current = nextStreams;
      setStreams(previous => { const next = { ...previous }; delete next[conversation.id]; return next; });
      setChatErrors(previous => ({ ...previous, [conversation.id]: errorMessage(failure) }));
    }
  };
  const activeServers = snapshot.servers.filter(server => ['running', 'ready'].includes(server.state));
  const runningDownloads = snapshot.downloads.filter(download => ['downloading', 'running', 'queued'].includes(downloadState(download)));
  const activeView = navigation.find(item => item.id === view)!;
  const serviceAvailable = connected && eventConnection === 'connected';
  const apiRunning = snapshot.api?.running === true;
  const startingServers = snapshot.servers.some(server => ['starting', 'loading'].includes(server.state));
  const inferenceReady = serviceAvailable && apiRunning && activeServers.length > 0;
  const serviceOffline = !connected && !loading && eventConnection !== 'disconnected';
  const connectionState = inferenceReady ? 'online' : serviceOffline || serviceAvailable && snapshot.api?.error ? 'offline' : 'idle';
  const inferenceStatus = !apiRunning ? snapshot.api?.error ? 'inference unavailable' : 'inference stopped' : activeServers.length ? `${activeServers.length} model${activeServers.length > 1 ? 's' : ''} serving` : startingServers ? 'models starting' : 'no models running';
  const connectionLabel = eventConnection === 'disconnected' ? 'Studio updates reconnecting' : !connected ? loading ? 'Connecting to Studio service' : 'Studio service offline' : eventConnection === 'connecting' ? 'Connecting to Studio updates' : `Studio connected · ${inferenceStatus}`;
  const connectionFailure = !connected && !loading || eventConnection === 'disconnected';

  return <div className="app-shell">
    <nav className="app-rail" aria-label="Main navigation">
      <button className="brand-mark" title="Locals Studio" aria-label="Locals Studio home" onClick={() => setView('chat')}><BrandIcon /></button>
      <div className="rail-items">{navigation.map(item => <button key={item.id} className={`rail-item ${view === item.id ? 'active' : ''}`} onClick={() => setView(item.id)} title={item.name} aria-label={item.name} aria-current={view === item.id ? 'page' : undefined}><item.icon size={21} /><span>{item.name === 'Model server' ? 'Server' : item.name === 'My models' ? 'Models' : item.name}</span>{item.id === 'search' && runningDownloads.length > 0 && <i className="rail-count">{runningDownloads.length}</i>}</button>)}</div>
      <div className="rail-bottom"><button className="icon-button" title={`Switch to ${theme === 'dark' ? 'light' : 'dark'} theme`} aria-label="Toggle theme" onClick={() => updateAppearance({ theme: theme === 'dark' ? 'light' : 'dark' })}>{theme === 'dark' ? <Sun size={20} /> : <Moon size={20} />}</button><button className={`icon-button ${settings ? 'selected' : ''}`} title="Settings" aria-label="Settings" onClick={() => { setSettingsTab('general'); setSettings(true); }}><Settings2 size={20} /></button><div className="avatar">L</div></div>
    </nav>
    <div className="workspace">
      <header className="app-header"><div className="header-breadcrumb"><span className="wordmark">locals<span>studio</span></span><span className="breadcrumb-slash">/</span><activeView.icon size={15} /><span>{activeView.name}</span></div><div className="header-actions"><span className="connection" role="status" aria-label="Studio status" data-state={connectionState} title="Studio management stays connected when inference is stopped. Green means the inference API and at least one model are ready."><i className={connectionState} />{connectionLabel}</span></div></header>
      {connectionFailure && <div className="connection-banner" role="alert"><AlertCircle size={16} /><span>{eventConnection === 'disconnected' ? eventError : error || 'Local service is unavailable.'}</span><button onClick={() => { reconnectEvents.current(); void refresh(); }}><RefreshCw size={13} /> Retry</button></div>}
      <main className="main-view">
        {<div className="view-pane" hidden={view !== 'chat'}><ChatView active={view === 'chat'} snapshot={snapshot} streams={streams} errors={chatErrors} onSend={send} onSave={saveConversation} onAction={action} onServer={() => setView('server')} /></div>}
        {view === 'server' && <ServerView snapshot={snapshot} selectedProfileId={selectedServerProfile} onSelectProfile={setSelectedServerProfile} onAction={action} onLibrary={() => setView('library')} />}
        {view === 'library' && <LibraryView snapshot={snapshot} onAction={action} onSearch={() => setView('search')} />}
        {view === 'search' && <SearchView snapshot={snapshot} onAction={action} />}
      </main>
      <footer className="status-bar"><span><i className={connectionState} />{!serviceAvailable ? 'Service status unavailable' : !apiRunning ? snapshot.api?.error ? 'Inference API unavailable' : 'Inference server stopped' : activeServers.length ? `${activeServers.length} model${activeServers.length > 1 ? 's' : ''} serving` : startingServers ? 'Models starting' : 'No models running'}</span><div>{runningDownloads.length > 0 && <button onClick={() => setView('search')}><ArrowDownToLine size={12} />{runningDownloads.length} download{runningDownloads.length > 1 ? 's' : ''}</button>}<span><Cpu size={12} />{snapshot.telemetry.system?.ram_used_bytes != null ? `${bytes(snapshot.telemetry.system.ram_used_bytes)} memory` : 'Local inference'}</span><span className="version">v0.1.0</span></div></footer>
    </div>
    {notice && <div className="toast" role="alert"><AlertCircle size={17} /><span>{notice}</span><button className="icon-button" aria-label="Dismiss notification" onClick={() => setNotice('')}><X size={15} /></button></div>}
    {settings && <SettingsModal snapshot={snapshot} appearance={appearance} onAppearanceChange={updateAppearance} tab={settingsTab} onTabChange={setSettingsTab} onClose={() => setSettings(false)} onAction={action} />}
  </div>;
}

function SettingsModal({ snapshot, appearance, onAppearanceChange, tab, onTabChange, onClose, onAction }: { snapshot: Snapshot; appearance: Appearance; onAppearanceChange: (patch: Partial<Appearance>) => void; tab: SettingsTab; onTabChange: (tab: SettingsTab) => void; onClose: () => void; onAction: (method: string, params?: JsonObject) => Promise<any> }) {
  const [saving, setSaving] = useState(false);
  const [busy, setBusy] = useState('');
  const [error, setError] = useState('');
  const [gatewayPort, setGatewayPort] = useState(String(snapshot.config.api?.port ?? 8321));
  const [gatewayHost, setGatewayHost] = useState(snapshot.config.api?.host ?? '127.0.0.1');
  const [allowedHosts, setAllowedHosts] = useState((snapshot.config.api?.allowed_hosts ?? []).join('\n'));
  const [allowedOrigins, setAllowedOrigins] = useState((snapshot.config.api?.allowed_origins ?? []).join('\n'));
  const [libraryPaths, setLibraryPaths] = useState((snapshot.config.library_paths ?? []).join('\n'));
  const [menuBarEnabled, setMenuBarEnabled] = useState(Boolean(snapshot.config.desktop?.menu_bar_enabled ?? false));
  const [saved, setSaved] = useState(false);
  const [workerRestartRequired, setWorkerRestartRequired] = useState(false);
  const runtime = snapshot.runtime;
  const install = async (method: string) => { setBusy(method); setError(''); try { await onAction(method); } catch (failure) { setError(errorMessage(failure)); } finally { setBusy(''); } };
  return <Modal title={tab === 'about' ? 'About Locals Studio' : 'Studio settings'} onClose={onClose}>
    <div className="settings-modal-layout">
    <div className="settings-tabs" role="tablist" aria-label="Settings sections"><button id="settings-general-tab" role="tab" aria-selected={tab === 'general'} aria-controls="settings-general-panel" onClick={() => onTabChange('general')}>General</button><button id="settings-about-tab" role="tab" aria-label="About Locals Studio" aria-selected={tab === 'about'} aria-controls="settings-about-panel" onClick={() => onTabChange('about')}>About</button></div>
    {tab === 'about' ? <section id="settings-about-panel" className="settings-panel" role="tabpanel" aria-labelledby="settings-about-tab"><div className="settings-panel-scroll"><AboutPanel /></div><footer className="modal-footer"><button onClick={onClose}>Close</button></footer></section> : <section id="settings-general-panel" className="settings-panel" role="tabpanel" aria-labelledby="settings-general-tab"><div className="settings-panel-scroll"><div className="settings-body"><SettingsAppearance value={appearance} onChange={onAppearanceChange} /><div className="section-label">DESKTOP</div><label className="desktop-preference"><input type="checkbox" aria-label="Show menu bar icon" checked={menuBarEnabled} onChange={event => { setMenuBarEnabled(event.target.checked); setSaved(false); }} /><span><strong>Show menu bar icon</strong><small>Keep your selected models and Start/Stop server controls available in the macOS menu bar when the window is closed.</small></span></label><div className="section-label">LOCALS RUNTIME</div><div className="runtime-card"><div><span className="runtime-icon"><Radio size={20} /></span><div><strong>Locals</strong><small>{runtime.installation?.locals_version ? `Locals ${runtime.installation.locals_version} · Python ${runtime.installation.python_version}` : `Python ${runtime.required_python ?? '3.12'} · Locals ${runtime.required_locals ?? '0.2.1'}`}</small></div></div><span className={`tag ${runtime.installed || runtime.available ? 'green' : ''}`}>{runtime.installed || runtime.available ? 'Installed' : 'Setup required'}</span></div>
      {(runtime.phase || runtime.logs?.length > 0) && <div className="runtime-progress"><span>{runtime.phase?.replaceAll('_', ' ')}</span>{runtime.logs?.length > 0 && <pre role="log" aria-label="Runtime installation output">{runtime.logs.slice(-8).join('\n')}</pre>}</div>}<div className="button-row"><button disabled={!!busy} onClick={() => void install('runtime.install')}>{busy === 'runtime.install' ? <Spinner /> : <ArrowDownToLine size={15} />} Install runtime</button><button disabled={!!busy} onClick={() => void install('runtime.update')}>{busy === 'runtime.update' ? <Spinner /> : <RefreshCw size={14} />} Update</button></div>
      <div className="section-label">NETWORK & STORAGE</div><div className="two-fields"><label className="field">API bind address<input value={gatewayHost} onChange={event => setGatewayHost(event.target.value)} placeholder="127.0.0.1" /></label><label className="field">Common API port<input type="number" min="1024" max="65535" value={gatewayPort} onChange={event => setGatewayPort(event.target.value)} /></label></div><label className="field">Allowed hosts (optional)<textarea aria-label="Allowed hosts" rows={2} value={allowedHosts} onChange={event => setAllowedHosts(event.target.value)} placeholder="One hostname per line" /><small>Leave blank to allow any host. Otherwise, enter one hostname per line.</small></label><label className="field">Allowed browser origins (optional)<textarea aria-label="Allowed browser origins" rows={2} value={allowedOrigins} onChange={event => setAllowedOrigins(event.target.value)} placeholder="https://your-app.example" /><small>Leave blank to allow any browser origin. Otherwise, enter one origin per line. Restart running model servers after changing this list.</small></label><label className="field">Additional model libraries<textarea rows={3} value={libraryPaths} onChange={event => setLibraryPaths(event.target.value)} placeholder="/Users/you/models" /><small>One path per line. Hugging Face and LM Studio caches are also scanned.</small></label>
      {error && <p className="inline-error" role="alert">{error}</p>}
      {workerRestartRequired && <p className="warning-message" role="status">Settings saved. Restart running model servers in Model server to apply the updated browser origins.</p>}
    </div></div><footer className="modal-footer"><span className="subtle">{saved && <><Check size={14} /> Settings saved</>}</span><button className="primary" disabled={saving} onClick={async () => { setSaving(true); setSaved(false); setError(''); try { const port = Number(gatewayPort); if (!Number.isInteger(port) || port < 1024 || port > 65535) throw new Error('Choose a port from 1024 to 65535.'); const lines = (value: string) => value.split('\n').map(line => line.trim()).filter(Boolean); const result = await onAction('config.set', { config: { ...snapshot.config, api: { ...snapshot.config.api, host: gatewayHost.trim(), port, allowed_hosts: lines(allowedHosts), allowed_origins: lines(allowedOrigins) }, library_paths: lines(libraryPaths), desktop: { ...snapshot.config.desktop, menu_bar_enabled: menuBarEnabled } } }); setWorkerRestartRequired(Boolean(result.worker_restart_required)); if (result.api?.error) { setError(`Settings were saved, but the inference API is unavailable: ${result.api.error} Choose an available API port or bind address, then save again.`); } else { setSaved(true); } } catch (failure) { setError(errorMessage(failure)); } finally { setSaving(false); } }}>{saving && <Spinner />} Save settings</button></footer></section>}
    </div>
  </Modal>;
}
