import { useEffect, useRef, useState } from 'react';
import { ArrowDownToLine, ArrowUpRight, Box, Check, ChevronRight, Code2, FolderOpen, FolderPlus, MemoryStick, MessageSquare, MoreHorizontal, Play, Plus, RefreshCw, Save, Search, Settings2, Square, Star, Trash2, X } from 'lucide-react';
import type { JsonObject, ModelRecord, Profile, Snapshot } from '../types';
import { bytes, parseEnvironment, parseOptions } from '../utils.mjs';
import { Empty, Modal, Spinner } from '../components';
import { errorMessage, request } from '../api';
import ModelLogo from '../ModelLogo';
import { FAVORITE_MODELS_KEY, loadFavoriteModels } from '../model-favorites';

const FAVORITES_ONLY_KEY = 'studio.library.favoritesOnly';

interface Props { snapshot: Snapshot; onAction: (method: string, params?: JsonObject) => Promise<any>; onSearch: () => void; onUseInChat?: (profile: Profile) => void; }
export default function LibraryView({ snapshot, onAction, onSearch, onUseInChat }: Props) {
  const [query, setQuery] = useState('');
  const [favoriteModelIds, setFavoriteModelIds] = useState(loadFavoriteModels);
  const [favoritesOnly, setFavoritesOnly] = useState(() => {
    try { return localStorage.getItem(FAVORITES_ONLY_KEY) === 'true'; }
    catch { return false; }
  });
  const favoritesFilter = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    try { localStorage.setItem(FAVORITE_MODELS_KEY, JSON.stringify([...favoriteModelIds])); }
    catch { /* Keep favorites usable when preference storage is unavailable. */ }
  }, [favoriteModelIds]);
  const toggleFavorite = (id: string) => {
    // Keep keyboard focus in the toolbar when the filtered row disappears.
    if (favoritesOnly && favoriteModelIds.has(id)) favoritesFilter.current?.focus();
    setFavoriteModelIds(previous => {
      const next = new Set(previous);
      if (next.has(id)) next.delete(id); else next.add(id);
      return next;
    });
  };
  const changeFavoritesOnly = (value: boolean) => {
    setFavoritesOnly(value);
    try { localStorage.setItem(FAVORITES_ONLY_KEY, String(value)); }
    catch { /* Keep filtering usable when preference storage is unavailable. */ }
  };
  const [selectedId, setSelectedId] = useState('');
  const [importing, setImporting] = useState(false);
  const [importPath, setImportPath] = useState('');
  const [busy, setBusy] = useState('');
  const [error, setError] = useState('');
  const [remove, setRemove] = useState<ModelRecord | null>(null);
  const [tab, setTab] = useState<'overview' | 'configuration'>('overview');
  const profileCache = useRef<ProfileCache>({ saved: new Map(), pending: new Map() });
  const latest = useRef(snapshot);
  latest.current = snapshot;
  const ensureProfile = async (model: ModelRecord): Promise<Profile> => {
    const existing = actionProfiles(model, latest.current)[0];
    if (existing) return existing;
    const cached = profileCache.current.saved.get(model.id);
    if (cached) return cached;
    const pending = profileCache.current.pending.get(model.id);
    if (pending) return pending.promise;
    const used = new Set([
      ...latest.current.profiles.map(profile => profile.alias),
      ...[...profileCache.current.saved.values()].map(profile => profile.alias),
      ...[...profileCache.current.pending.values()].map(entry => entry.profile.alias),
    ]);
    const base = model.name.normalize('NFKD').replace(/[\u0300-\u036f]/g, '').replace(/[^A-Za-z0-9._-]+/g, '-').replace(/^-+|-+$/g, '').toLowerCase() || 'model';
    let alias = base;
    for (let suffix = 2; used.has(alias); suffix += 1) alias = `${base}-${suffix}`;
    const profile: Profile = { id: crypto.randomUUID(), model_id: model.id, alias, selected: false, options: {}, env: {} };
    const promise = onAction('profiles.save', { profile }).then(result => {
      const saved = result?.id && result?.model_id && result?.alias ? result as Profile : profile;
      if (latest.current.models.some(item => item.id === model.id) && !latest.current.profiles.some(profile => profile.id === saved.id)) profileCache.current.saved.set(model.id, saved);
      return saved;
    }).finally(() => { profileCache.current.pending.delete(model.id); });
    profileCache.current.pending.set(model.id, { profile, promise });
    return promise;
  };
  const selected = snapshot.models.find(model => model.id === selectedId);
  const favoriteCount = snapshot.models.filter(model => favoriteModelIds.has(model.id)).length;
  const filtered = snapshot.models.filter(model => (!favoritesOnly || favoriteModelIds.has(model.id)) && `${model.name} ${model.repo_id ?? ''} ${model.architecture ?? ''}`.toLowerCase().includes(query.toLowerCase()));
  const totalSize = snapshot.models.reduce((sum, model) => sum + (model.weights_bytes ?? 0), 0);
  const run = async (method: string, params: JsonObject = {}) => { setError(''); setBusy(method); try { return await onAction(method, params); } catch (failure) { setError(errorMessage(failure)); throw failure; } finally { setBusy(''); } };
  useEffect(() => { if (selectedId && !snapshot.models.some(model => model.id === selectedId)) setSelectedId(''); }, [snapshot.models, selectedId]);
  useEffect(() => {
    for (const [modelId, saved] of profileCache.current.saved) {
      if (snapshot.profiles.some(profile => profile.id === saved.id) || !snapshot.models.some(model => model.id === modelId)) profileCache.current.saved.delete(modelId);
    }
  }, [snapshot.profiles, snapshot.models]);

  return <div className="library-view"><div className="page-content library-content"><div className="page-title"><div><div className="eyebrow">YOUR LOCAL COLLECTION</div><h1>My models <span className="count-badge">{snapshot.models.length}</span></h1><p>A home for the models on your machine.</p></div><div className="button-row"><button disabled={!!busy} onClick={() => void run('models.scan').catch(() => {})}>{busy === 'models.scan' ? <Spinner size={15} /> : <RefreshCw size={15} />} Rescan</button><button onClick={() => setImporting(true)}><FolderPlus size={15} /> Import</button><button className="primary" onClick={onSearch}><Search size={15} /> Find a model</button></div></div>
      <div className="library-summary"><div className="collection-icon"><Box size={21} /></div><div><strong>{snapshot.models.length} model{snapshot.models.length === 1 ? '' : 's'} in your library</strong><span>{bytes(totalSize)} of model weights <span className="dot-separator">·</span> MLX for Apple Silicon</span></div><span className="tag green">ON DEVICE</span></div>
      <div className="library-filter"><div className="search-input"><Search size={16} /><input aria-label="Filter local models" value={query} onChange={event => setQuery(event.target.value)} placeholder="Filter your models…" /><kbd>⌕</kbd></div><button ref={favoritesFilter} className="library-favorites-filter" aria-pressed={favoritesOnly} onClick={() => changeFavoritesOnly(!favoritesOnly)}><Star size={14} fill={favoritesOnly ? 'currentColor' : 'none'} aria-hidden="true" />Favorites only</button><span className="subtle library-visible-count" role="status">{filtered.length} model{filtered.length === 1 ? '' : 's'}</span></div>
      {error && <div className="inline-error">{error}</div>}
      {!snapshot.models.length ? <Empty icon={<Box size={29} />} title="Your collection starts here" action={<div className="button-row"><button onClick={() => setImporting(true)}><FolderPlus size={15} /> Import local model</button><button className="primary" onClick={onSearch}><Search size={15} /> Discover models</button></div>}>Download an MLX model from Hugging Face, or bring one you already have.</Empty>
        : favoritesOnly && !favoriteCount ? <Empty icon={<Star size={25} />} title="No favorite models yet" action={<button onClick={() => changeFavoritesOnly(false)}>Show all models</button>}>Use the star next to a model to add it to your favorites.</Empty>
        : !filtered.length ? <Empty icon={<Search size={25} />} title="No matching models">{favoritesOnly ? 'No favorite matches this search. Try another name or turn off Favorites only.' : 'Try another name or architecture.'}</Empty>
        : <div className="model-library-list">{filtered.map(model => {
          const profiles = snapshot.profiles.filter(profile => profile.model_id === model.id);
          const running = snapshot.servers.some(server => server.model_id === model.id && ['ready', 'running', 'starting'].includes(server.state));
          const favorite = favoriteModelIds.has(model.id);
          const favoriteLabel = `${favorite ? 'Remove' : 'Add'} ${model.name} ${favorite ? 'from' : 'to'} favorites`;
          return <div className="library-model-row" key={model.id}>
            <button className={`library-model ${selectedId === model.id ? 'selected' : ''}`} onClick={() => { setSelectedId(model.id); setTab('overview'); }}><ModelLogo repoId={model.repo_id} name={model.name} size={48} className="library-model-icon" /><div className="library-model-info"><div><strong>{model.name}</strong>{running && <span className="tag green">RUNNING</span>}{!model.compatible && <span className="tag warning">CHECK COMPATIBILITY</span>}</div><span>{model.repo_id ?? model.path}</span><div className="model-tags"><span>MLX</span>{model.architecture && <span>{model.architecture}</span>}{model.quantization && <span>{model.quantization}</span>}{profiles.length > 0 && <span>{profiles.length} profile{profiles.length === 1 ? '' : 's'}</span>}</div></div><div className="library-size"><strong>{bytes(model.weights_bytes)}</strong><small>{model.source === 'studio' ? 'Downloaded' : 'Local model'}</small></div><ChevronRight size={16} /></button>
            <button className="icon-button model-favorite-button" aria-label={favoriteLabel} title={favoriteLabel} aria-pressed={favorite} onClick={() => toggleFavorite(model.id)}><Star size={18} fill={favorite ? 'currentColor' : 'none'} aria-hidden="true" /></button>
          </div>;
        })}</div>}
      <div className="library-tip"><FolderOpen size={17} /><div><strong>Already have models?</strong><span>Studio can reference models from Hugging Face, LM Studio, or any local folder without copying the weights.</span></div><button className="text-button" onClick={() => setImporting(true)}>Add a folder <ArrowUpRight size={14} /></button></div>
    </div>
    {selected && <aside className="model-inspector"><div className="side-heading"><span>Model details</span><button className="icon-button" aria-label="Close model details" onClick={() => setSelectedId('')}><X size={17} /></button></div><div className="inspector-model-heading"><ModelLogo repoId={selected.repo_id} name={selected.name} size={48} className="library-model-icon" /><h2>{selected.name}</h2><p>{selected.repo_id ?? selected.architecture ?? 'Local MLX model'}</p><span className="tag">MLX</span></div><div className="inspector-tabs"><button className={tab === 'overview' ? 'active' : ''} onClick={() => setTab('overview')}>Overview</button><button className={tab === 'configuration' ? 'active' : ''} onClick={() => setTab('configuration')}>Server profiles</button></div>
      {tab === 'overview' ? <div className="inspector-body"><div className="detail-row"><span>Architecture</span><strong>{selected.architecture ?? 'Unknown'}</strong></div><div className="detail-row"><span>Quantization</span><strong>{selected.quantization ?? 'Unknown'}</strong></div><div className="detail-row"><span>Weight size</span><strong>{bytes(selected.weights_bytes)}</strong></div><div className="detail-row"><span>Context length</span><strong>{selected.context_length?.toLocaleString() ?? 'Model default'}</strong></div><div className="detail-row"><span>Source</span><strong>{selected.source}</strong></div><label className="field path-detail">Model directory<code>{selected.path}</code></label>{selected.revision && <label className="field path-detail">Revision<code>{selected.revision}</code></label>}<ModelQuickActions key={selected.id} model={selected} snapshot={snapshot} cache={profileCache.current} ensureProfile={ensureProfile} onAction={onAction} onUseInChat={onUseInChat} /><button className="primary full-width" onClick={() => setTab('configuration')}><Settings2 size={15} /> Configure server</button><button className="text-button danger" onClick={() => setRemove(selected)}><Trash2 size={14} /> Remove from library</button></div> : <ProfileEditor key={selected.id} model={selected} profiles={snapshot.profiles.filter(profile => profile.model_id === selected.id)} onAction={onAction} />}
    </aside>}
    {importing && <Modal title="Import a local model" onClose={() => setImporting(false)}><form onSubmit={event => { event.preventDefault(); if (!importPath.trim()) return; void run('models.import', { path: importPath.trim() }).then(() => { setImporting(false); setImportPath(''); }).catch(() => {}); }}><div className="settings-body"><div className="import-art"><FolderOpen size={32} /></div><p className="subtle">Point Studio to a complete MLX model directory. Your model stays in its current location.</p><label className="field">Model folder<input autoFocus value={importPath} onChange={event => setImportPath(event.target.value)} placeholder="/Users/you/models/my-model" required /><small>Include config.json, tokenizer files, and all safetensors shards.</small></label>{error && <p className="inline-error">{error}</p>}</div><footer className="modal-footer"><button type="button" onClick={() => setImporting(false)}>Cancel</button><button className="primary" disabled={!!busy || !importPath.trim()}>{busy ? <Spinner /> : <FolderPlus size={15} />} Import model</button></footer></form></Modal>}
    {remove && <Modal title="Remove model from library?" onClose={() => setRemove(null)}><div className="settings-body"><p>Remove <strong>{remove.name}</strong> from Studio's library?</p><p className="subtle">This unregisters the model. Existing model files stay on disk.</p>{error && <p className="inline-error">{error}</p>}</div><footer className="modal-footer"><button onClick={() => setRemove(null)}>Cancel</button><button className="danger-button" disabled={!!busy} onClick={() => void run('models.remove', { id: remove.id }).then(() => { setRemove(null); setSelectedId(''); }).catch(() => {})}>{busy ? <Spinner /> : <Trash2 size={14} />} Remove model</button></footer></Modal>}
  </div>;
}

type ProfileCache = {
  saved: Map<string, Profile>;
  pending: Map<string, { profile: Profile; promise: Promise<Profile> }>;
};
const OCCUPIED_STATES = ['ready', 'running', 'starting', 'loading', 'draining', 'stopping'];
function actionProfiles(model: ModelRecord, snapshot: Snapshot): Profile[] {
  const rank = (profile: Profile) => {
    const state = snapshot.servers.find(server => server.profile_id === profile.id)?.state;
    return state === 'ready' || state === 'running' ? 0 : state === 'starting' || state === 'loading' ? 1 : profile.selected ? 2 : 3;
  };
  return snapshot.profiles.filter(profile => profile.model_id === model.id).sort((a, b) => rank(a) - rank(b) || a.alias.localeCompare(b.alias, undefined, { numeric: true, sensitivity: 'base' }) || a.id.localeCompare(b.id));
}

function ModelQuickActions({ model, snapshot, cache, ensureProfile, onAction, onUseInChat }: {
  model: ModelRecord; snapshot: Snapshot; cache: ProfileCache; ensureProfile: (model: ModelRecord) => Promise<Profile>;
  onAction: Props['onAction']; onUseInChat: Props['onUseInChat'];
}) {
  const profiles = actionProfiles(model, snapshot);
  const cached = cache.saved.get(model.id);
  if (cached && !profiles.some(profile => profile.id === cached.id)) profiles.push(cached);
  const [chosenId, setChosenId] = useState(() => profiles[0]?.id ?? '');
  const chosen = profiles.find(profile => profile.id === chosenId) ?? profiles[0];
  const [busy, setBusy] = useState<'start' | 'chat' | 'reload' | 'stop' | null>(null);
  const [error, setError] = useState('');
  const [review, setReview] = useState<{ profile: Profile; estimate: JsonObject; token: object; kind: 'start' | 'reload' } | null>(null);
  const [started, setStarted] = useState<Record<string, { state: string; before: string }>>({});
  const alive = useRef(true);
  const operation = useRef<object | null>(null);
  const latest = useRef(snapshot);
  latest.current = snapshot;
  useEffect(() => {
    alive.current = true;
    return () => { alive.current = false; operation.current = null; };
  }, []);
  const valid = (token: object) => alive.current && operation.current === token;
  const serverStamp = (id: string) => {
    const server = latest.current.servers.find(server => server.profile_id === id);
    return JSON.stringify(server ? [server.state, server.pid ?? null, server.port ?? null, server.started_at ?? null] : null);
  };
  useEffect(() => {
    setStarted(previous => {
      const current = Object.fromEntries(Object.entries(previous).filter(([id, result]) => serverStamp(id) === result.before));
      return Object.keys(current).length === Object.keys(previous).length ? previous : current;
    });
  }, [snapshot.servers, started]);
  const stateFor = (id: string) => started[id]?.before === serverStamp(id) ? started[id].state : latest.current.servers.find(server => server.profile_id === id)?.state ?? 'stopped';
  const occupied = (id: string) => OCCUPIED_STATES.includes(stateFor(id));
  const state = chosen ? stateFor(chosen.id) : 'stopped';
  const cancel = () => { operation.current = null; setBusy(null); setReview(null); };
  const startProfile = async (profile: Profile, token: object, override = false, afterStop = false) => {
    if (!valid(token) || !afterStop && occupied(profile.id) || !latest.current.models.some(item => item.id === model.id && item.compatible)) return;
    setBusy(afterStop ? 'reload' : 'start');
    const before = serverStamp(profile.id);
    const result = await onAction('server.start', { id: profile.id, ...(override ? { allow_memory_overcommit: true } : {}) });
    if (valid(token)) {
      const status = result?.servers?.find((server: JsonObject) => server.profile_id === profile.id);
      if (status?.state) setStarted(previous => ({ ...previous, [profile.id]: { state: status.state, before } }));
    }
  };
  const reloadable = (id: string) => ['ready', 'running'].includes(stateFor(id));
  const stoppable = (id: string) => ['ready', 'running', 'starting', 'loading'].includes(stateFor(id));
  const stopProfile = async (profile: Profile, token: object) => {
    const before = serverStamp(profile.id);
    const result = await onAction('server.stop', { id: profile.id });
    if (valid(token)) {
      const status = result?.servers?.find((server: JsonObject) => server.profile_id === profile.id);
      if (status?.state) setStarted(previous => ({ ...previous, [profile.id]: { state: status.state, before } }));
    }
    return result;
  };
  const reloadProfile = async (profile: Profile, token: object, override = false) => {
    if (!valid(token) || !reloadable(profile.id) || !latest.current.models.some(item => item.id === model.id && item.compatible)) return;
    setBusy('reload');
    const result = await stopProfile(profile, token);
    if (!valid(token)) return;
    const status = result?.servers?.find((server: JsonObject) => server.profile_id === profile.id);
    if (status?.state && status.state !== 'stopped') throw new Error('The model has not stopped yet. Wait for it to stop before starting it again.');
    // A successful targeted stop is authoritative even if the next snapshot still
    // describes the old process. Never bypass the occupied check for a normal start.
    await startProfile(profile, token, override, true);
  };
  const act = async (kind: 'start' | 'chat' | 'reload' | 'stop') => {
    if (operation.current || kind !== 'stop' && !model.compatible
      || kind === 'chat' && !onUseInChat
      || kind === 'start' && chosen && occupied(chosen.id)
      || kind === 'reload' && (!chosen || !reloadable(chosen.id))
      || kind === 'stop' && (!chosen || !stoppable(chosen.id))) return;
    const token = {};
    operation.current = token;
    setBusy(kind); setError('');
    let awaitingReview = false;
    try {
      const profile = chosen ?? await ensureProfile(model);
      if (!valid(token) || !latest.current.models.some(item => item.id === model.id && (kind === 'stop' || item.compatible))) return;
      setChosenId(profile.id);
      if (kind === 'chat') { onUseInChat?.(profile); return; }
      if (kind === 'stop') {
        if (stoppable(profile.id)) await stopProfile(profile, token);
        return;
      }
      if (kind === 'reload' ? !reloadable(profile.id) : occupied(profile.id)) return;
      const estimate = await request('memory.estimate', { id: profile.id });
      if (!valid(token) || (kind === 'reload' ? !reloadable(profile.id) : occupied(profile.id)) || !latest.current.models.some(item => item.id === model.id && item.compatible)) return;
      if (estimate.requires_override) {
        awaitingReview = true;
        setReview({ profile, estimate, token, kind });
      } else if (kind === 'reload') await reloadProfile(profile, token);
      else await startProfile(profile, token);
    } catch (failure) { if (valid(token)) setError(errorMessage(failure)); }
    finally { if (valid(token)) { setBusy(null); if (!awaitingReview) operation.current = null; } }
  };
  const confirmStart = async () => {
    if (!review || !valid(review.token)) return;
    const { profile, kind } = review;
    // Revoke the review synchronously so an already queued second click cannot start twice.
    const next = {};
    operation.current = next;
    setReview(null); setBusy(kind);
    try {
      if (kind === 'reload') await reloadProfile(profile, next, true);
      else await startProfile(profile, next, true);
    }
    catch (failure) { if (valid(next)) setError(errorMessage(failure)); }
    finally { if (valid(next)) { operation.current = null; setBusy(null); } }
  };
  const label = ['ready', 'running'].includes(state) ? 'Running' : ['starting', 'loading'].includes(state) ? 'Starting…' : ['draining', 'stopping'].includes(state) ? 'Stopping…' : busy === 'start' ? 'Starting…' : 'Start model';
  return <div className="model-quick-actions">
    {profiles.length > 1 && <label className="field action-profile-selector">Server profile<select aria-label="Model action profile" value={chosen?.id ?? ''} onChange={event => { cancel(); setError(''); setChosenId(event.target.value); }}>{profiles.map(profile => <option key={profile.id} value={profile.id}>{profile.alias}</option>)}</select></label>}
    <button className="primary full-width" aria-label="Start model" disabled={!model.compatible || !!busy || !!review || OCCUPIED_STATES.includes(state)} onClick={() => void act('start')}>{busy === 'start' ? <Spinner size={15} /> : <Play size={15} />}{label}</button>
    <button className="full-width" aria-label="Use in new chat" disabled={!model.compatible || !!busy || !!review || !onUseInChat} onClick={() => void act('chat')}>{busy === 'chat' ? <Spinner size={15} /> : <MessageSquare size={15} />}Use in new chat</button>
    <div className="model-lifecycle-actions"><button className="full-width" aria-label="Reload model" disabled={!chosen || !model.compatible || !!busy || !!review || !reloadable(chosen.id)} onClick={() => void act('reload')}>{busy === 'reload' ? <Spinner size={15} /> : <RefreshCw size={15} />}Reload</button><button className="full-width stop-server" aria-label="Stop model" disabled={!chosen || !!busy || !!review || !stoppable(chosen.id)} onClick={() => void act('stop')}>{busy === 'stop' ? <Spinner size={15} /> : <Square size={15} />}Stop</button></div>
    {!model.compatible && <p className="subtle tiny">This model is incomplete or incompatible. Import a complete, supported MLX model to start it or use it in chat.</p>}
    {error && <p className="inline-error" role="alert">{error}</p>}
    {review && <Modal title="Review memory usage" onClose={cancel}><div className="settings-body"><div className="memory-review"><MemoryStick size={24} /><strong>{bytes(review.estimate.total_estimated_bytes)}</strong><span>estimated · {bytes(review.estimate.budget_bytes)} budget</span></div>{review.estimate.warnings?.map((warning: JsonObject) => <p className="warning-message" key={warning.code}>{warning.message}</p>)}<p className="subtle">{review.estimate.note}</p></div><footer className="modal-footer"><button onClick={cancel}>Cancel</button><button className="primary" onClick={() => void confirmStart()}>Start anyway</button></footer></Modal>}
  </div>;
}

function ProfileEditor({ model, profiles, onAction }: { model: ModelRecord; profiles: Profile[]; onAction: Props['onAction'] }) {
  const blank = (): Profile => ({ id: crypto.randomUUID(), model_id: model.id, alias: model.name.replace(/[^A-Za-z0-9._-]/g, '-'), selected: true, options: {}, env: {} });
  const [profile, setProfile] = useState<Profile>(profiles[0] ?? blank);
  const [optionsText, setOptionsText] = useState(JSON.stringify(profile.options, null, 2));
  const [envText, setEnvText] = useState(Object.entries(profile.env).map(([key, value]) => `${key}=${value}`).join('\n'));
  const [advanced, setAdvanced] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [saved, setSaved] = useState(false);
  const [fields, setFields] = useState<JsonObject[]>([]);
  const [schemaError, setSchemaError] = useState('');
  useEffect(() => { let alive = true; void request('options.schema').then(schema => { if (alive) setFields(schema.fields ?? []); }).catch(failure => { if (alive) setSchemaError(errorMessage(failure)); }); return () => { alive = false; }; }, []);
  const load = (next: Profile) => { setProfile(next); setOptionsText(JSON.stringify(next.options, null, 2)); setEnvText(Object.entries(next.env).map(([key, value]) => `${key}=${value}`).join('\n')); setError(''); setSaved(false); };
  const setOption = (key: string, value: string, type = 'number') => { const options = { ...profile.options }; if (value.trim() === '') delete options[key]; else options[key] = type === 'boolean' ? value === 'true' : type === 'string' ? value : Number(value); setProfile({ ...profile, options }); setOptionsText(JSON.stringify(options, null, 2)); setSaved(false); };
  const editJson = (value: string) => { setOptionsText(value); setSaved(false); try { const options = parseOptions(value); setProfile(previous => ({ ...previous, options })); } catch { /* Keep incomplete JSON editable until the user saves. */ } };
  return <form className="inspector-body profile-editor" onSubmit={async event => { event.preventDefault(); setBusy(true); setError(''); try { const options = parseOptions(optionsText); const env = parseEnvironment(envText); await onAction('profiles.save', { profile: { ...profile, alias: profile.alias.trim(), options, env } }); setSaved(true); } catch (failure) { setError(errorMessage(failure)); } finally { setBusy(false); } }}>
    <div className="profile-selector"><select aria-label="Server profile" value={profiles.some(item => item.id === profile.id) ? profile.id : ''} onChange={event => { const next = profiles.find(item => item.id === event.target.value); if (next) load(next); }}><option value="">New server profile</option>{profiles.map(item => <option value={item.id} key={item.id}>{item.alias}</option>)}</select><button type="button" className="icon-button" title="New server profile" aria-label="New server profile" onClick={() => load(blank())}><Plus size={16} /></button></div>
    <label className="field">API model name<input required value={profile.alias} onChange={event => { setProfile({ ...profile, alias: event.target.value }); setSaved(false); }} /><small>Clients use this alias in the model field.</small></label><div className="section-label">MEMORY & CONCURRENCY</div><label className="field">Maximum context<input type="number" min="1" step="1" placeholder="Auto" value={profile.options.max_context ?? ''} onChange={event => setOption('max_context', event.target.value)} /><small>Prompt and output tokens per request.</small></label><div className="two-fields"><label className="field">Prompt cache (GB)<input type="number" min="0" step="any" placeholder="Auto" value={profile.options.cache_gb ?? ''} onChange={event => setOption('cache_gb', event.target.value)} /><small>Blank for Auto. Use a positive size.</small></label><label className="field">Concurrent slots<input type="number" min="1" step="1" placeholder="Locals default" value={profile.options.slots ?? ''} onChange={event => setOption('slots', event.target.value)} /></label></div><label className="check-field"><input type="checkbox" checked={profile.selected} onChange={event => setProfile({ ...profile, selected: event.target.checked })} /> Include in “Start selected”</label>
    <button type="button" className="advanced-toggle" onClick={() => setAdvanced(!advanced)}><Code2 size={14} /> Advanced configuration <ChevronRight size={13} className={advanced ? 'rotate' : ''} /></button>{advanced && <><div className="section-label">LOCALS OPTIONS</div>{fields.filter(field => !['max_context', 'cache_gb', 'slots'].includes(field.key)).map(field => <label className="field" key={field.key}>{field.label ?? field.key}{field.type === 'boolean' ? <select value={profile.options[field.key] == null ? '' : String(profile.options[field.key])} onChange={event => setOption(field.key, event.target.value, 'boolean')}><option value="">Default{field.default != null ? ` (${field.default ? 'on' : 'off'})` : ''}</option><option value="true">On</option><option value="false">Off</option></select> : <input type={field.type === 'string' ? 'text' : 'number'} step={field.type === 'integer' ? '1' : 'any'} min={field.min} max={field.max} value={profile.options[field.key] ?? ''} placeholder={field.default == null ? 'Auto' : `Default: ${field.default}`} onChange={event => setOption(field.key, event.target.value, field.type)} />}<small>{field.description}</small></label>)}{schemaError && <p className="subtle tiny">Option controls unavailable. Configure options below.</p>}<div className="section-label">EXPERT CONFIGURATION</div><label className="field">Locals CLI options<textarea className="code-input" rows={8} value={optionsText} onChange={event => editJson(event.target.value)} spellCheck={false} /><small>JSON object of CLI flags. Use underscores or hyphens. Worker host, port, and model are managed by Studio.</small></label><label className="field">Environment variables<textarea className="code-input" rows={5} value={envText} onChange={event => { setEnvText(event.target.value); setSaved(false); }} placeholder="LOCALS_KV_BITS=8" spellCheck={false} /><small>One NAME=value per line.</small></label></>}
    {error && <p className="inline-error">{error}</p>}<button className="primary full-width" disabled={busy}>{busy ? <Spinner /> : saved ? <Check size={15} /> : <Save size={15} />}{saved ? 'Profile saved' : 'Save profile'}</button>{profiles.some(item => item.id === profile.id) && <button type="button" className="text-button danger" disabled={busy} onClick={async () => { setBusy(true); try { await onAction('profiles.remove', { id: profile.id }); load(blank()); } catch (failure) { setError(errorMessage(failure)); } finally { setBusy(false); } }}><Trash2 size={13} /> Delete profile</button>}<p className="subtle tiny">Saved changes apply when the server next starts.</p>
  </form>;
}
