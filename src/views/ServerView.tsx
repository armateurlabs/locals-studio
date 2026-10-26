import { useEffect, useMemo, useRef, useState } from 'react';
import { Activity, ArrowUpRight, Check, ChevronDown, CircleHelp, Cpu, Download, HardDrive, MemoryStick, Play, Plus, RefreshCw, Search, Server as ServerIcon, Square, Terminal, X } from 'lucide-react';
import type { JsonObject, Profile, Server, Snapshot } from '../types';
import { bytes, logLines } from '../utils.mjs';
import { CopyButton, Empty, Modal, Spinner, Status } from '../components';
import { errorMessage, request } from '../api';
import ModelLogo from '../ModelLogo';

interface Props { snapshot: Snapshot; onAction: (method: string, params?: JsonObject) => Promise<any>; onLibrary: () => void; selectedProfileId?: string; onSelectProfile?: (id: string) => void; }
const SELECTED_ONLY_KEY = 'studio.server.selectedOnly';
const SEARCH_KEY = 'studio.server.search';
const OCCUPIED_STATES = ['running', 'ready', 'starting', 'loading', 'draining', 'stopping'];

export default function ServerView({ snapshot, onAction, onLibrary, selectedProfileId, onSelectProfile }: Props) {
  // Saving a selection changes the store's modification order, not the display order.
  const profiles = useMemo(() => [...snapshot.profiles].sort((a, b) => a.alias.localeCompare(b.alias, undefined, { numeric: true, sensitivity: 'base' }) || a.id.localeCompare(b.id)), [snapshot.profiles]);
  const [busy, setBusy] = useState<string[]>([]);
  const [selectedOnly, setSelectedOnly] = useState(() => {
    try { return localStorage.getItem(SELECTED_ONLY_KEY) === 'true'; }
    catch { return false; }
  });
  const changeSelectedOnly = (value: boolean) => {
    setSelectedOnly(value);
    try { localStorage.setItem(SELECTED_ONLY_KEY, String(value)); }
    catch { /* Filtering still works when local preferences cannot be saved. */ }
  };
  const [searchQuery, setSearchQuery] = useState(() => {
    try { return localStorage.getItem(SEARCH_KEY) ?? ''; }
    catch { return ''; }
  });
  const changeSearch = (value: string) => {
    setSearchQuery(value);
    try { localStorage.setItem(SEARCH_KEY, value); }
    catch { /* Search remains usable when local preferences cannot be saved. */ }
  };
  const searchTerm = searchQuery.trim().toLocaleLowerCase();
  const [localLogId, setLocalLogId] = useState('');
  const logId = selectedProfileId ?? localLogId;
  const setLogId = onSelectProfile ?? setLocalLogId;
  const [logs, setLogs] = useState<string[]>([]);
  const [logsOpen, setLogsOpen] = useState(true);
  const [logHeight, setLogHeight] = useState(185);
  const logDrag = useRef<{ y: number; height: number } | null>(null);
  const requests = useRef(new Map<string, symbol>());
  const startEpoch = useRef(0);
  const [error, setError] = useState('');
  const [estimate, setEstimate] = useState<JsonObject | null>(null);
  const [pendingStart, setPendingStart] = useState<JsonObject | null>(null);
  const [lastEstimate, setLastEstimate] = useState<JsonObject | null>(null);
  const telemetry = snapshot.telemetry;
  const system = telemetry.system ?? {};
  const totals = telemetry.managed_totals ?? {};
  const active = snapshot.servers.filter(server => ['running', 'ready'].includes(server.state));
  const selected = profiles.filter(profile => profile.selected);
  const needsStart = (id: string) => !snapshot.servers.some(server => server.profile_id === id && OCCUPIED_STATES.includes(server.state));
  const selectedToStart = selected.filter(profile => needsStart(profile.id));
  const startBusy = busy.some(key => key === 'all' || key.startsWith('start:'));
  const visibleProfiles = (selectedOnly ? selected : profiles).filter(profile => {
    if (!searchTerm) return true;
    const model = snapshot.models.find(model => model.id === profile.model_id);
    return [profile.alias, model?.name, model?.repo_id].some(value => value?.toLocaleLowerCase().includes(searchTerm));
  });
  const selectedWorker = (telemetry.workers ?? []).find((worker: JsonObject) => worker.profile_id === logId);
  const selectedProfile = profiles.find(profile => profile.id === logId);
  const selectedModel = snapshot.models.find(model => model.id === selectedProfile?.model_id);
  const selectedServer = snapshot.servers.find(server => server.profile_id === logId);
  const selectedMemory = matchesWorker(selectedWorker, selectedServer) ? selectedWorker : undefined;
  const releaseEstimates = (telemetry.workers ?? []).filter((worker: JsonObject) => worker.pid != null).map(potentiallyReleasable);
  const missingWorkerTelemetry = snapshot.servers.some(server => server.pid != null && !(telemetry.workers ?? []).some((worker: JsonObject) => worker.profile_id === server.profile_id && worker.pid === server.pid));
  const totalReleasable = !missingWorkerTelemetry && releaseEstimates.every((value: number | null) => value != null) ? releaseEstimates.reduce((sum: number, value: number) => sum + value, 0) : null;
  const workerStateKey = snapshot.servers.map(server => `${server.profile_id}:${server.pid ?? ''}:${server.state}`).sort().join('|');
  // Polls return fresh arrays even when the model configuration has not changed.
  // Telemetry must not remove and recreate content beneath the current scroll position.
  const estimateKey = JSON.stringify([profiles, snapshot.models, workerStateKey, snapshot.config.memory_budget_bytes, snapshot.runtime.installation?.recommended_gpu_bytes]);
  const api = snapshot.api ?? {};
  const apiRunning = api.running === true;
  const canStopServer = apiRunning || (api.enabled ?? snapshot.config.api?.enabled) === true || snapshot.servers.some(server => server.pid != null || ['starting', 'loading', 'ready', 'running', 'draining'].includes(server.state)) || busy.some(key => key === 'all' || key.startsWith('start:')) || pendingStart != null;
  const [chosenEndpoint, setChosenEndpoint] = useState('');
  const endpoints: string[] = api.urls?.length ? api.urls : [`http://${snapshot.config.api?.host ?? '127.0.0.1'}:${snapshot.config.api?.port ?? 8321}/v1`];
  const address = endpoints.includes(chosenEndpoint) ? chosenEndpoint : endpoints[0];
  useEffect(() => { if (!profiles.some(profile => profile.id === logId)) setLogId(profiles[0]?.id ?? ''); }, [profiles, logId, setLogId]);
  useEffect(() => {
    let alive = true;
    setLogs([]);
    const read = async () => { if (!logId || !logsOpen) return; try { const result = await request('server.logs', { id: logId }); if (alive) setLogs([...(result.truncated ? ['Earlier log lines omitted. Showing the latest output.'] : []), ...logLines(result)]); } catch (failure) { if (alive) setLogs([`Could not read server logs: ${errorMessage(failure)}`]); } };
    void read(); const timer = window.setInterval(() => void read(), 3000);
    return () => { alive = false; window.clearInterval(timer); };
  }, [logId, logsOpen]);
  useEffect(() => { let alive = true; void request('memory.estimate').then(value => { if (alive) setLastEstimate(value); }).catch(() => {}); return () => { alive = false; }; }, [estimateKey]);

  const execute = async (method: string, params: JsonObject, key: string) => {
    if (requests.current.has(key)) return;
    const token = Symbol(key);
    requests.current.set(key, token);
    setError(''); setBusy(previous => [...previous, key]);
    try { await onAction(method, params); }
    catch (failure) { setError(errorMessage(failure)); }
    finally { if (requests.current.get(key) === token) { requests.current.delete(key); setBusy(previous => previous.filter(item => item !== key)); } }
  };
  const start = async (params: JsonObject, key: string) => {
    if (pendingStart || [...requests.current.keys()].some(item => item === 'all' || item === 'global-stop' || item.startsWith('start:'))) return;
    if (params.all ? apiRunning && selectedToStart.length === 0 : !needsStart(params.id)) return;
    const token = Symbol(key);
    const epoch = startEpoch.current;
    requests.current.set(key, token);
    setError(''); setBusy(previous => [...previous, key]);
    try {
      if (!(params.all && selected.length === 0)) {
        const result = await request('memory.estimate', params);
        if (epoch !== startEpoch.current) return;
        if (result.requires_override) { setEstimate(result); setPendingStart({ ...params, key, epoch }); return; }
      }
      if (epoch !== startEpoch.current) return;
      await onAction('server.start', params);
    } catch (failure) { if (epoch === startEpoch.current) setError(errorMessage(failure)); }
    finally { if (requests.current.get(key) === token) { requests.current.delete(key); setBusy(previous => previous.filter(item => item !== key)); } }
  };
  const stopServer = async () => {
    if (requests.current.has('global-stop')) return;
    startEpoch.current += 1;
    setEstimate(null); setPendingStart(null);
    for (const key of requests.current.keys()) if (key === 'all' || key.startsWith('start:')) requests.current.delete(key);
    setBusy(previous => previous.filter(key => key !== 'all' && !key.startsWith('start:')));
    await execute('server.stop', { all: true }, 'global-stop');
  };
  const gauge = system.ram_total_bytes ? Math.min(100, system.ram_used_bytes / system.ram_total_bytes * 100) : 0;

  return <div className="server-view">
    <div className="page-content server-content"><div className="page-title"><div><div className="eyebrow">LOCAL INFERENCE</div><h1>Model server <span className="count-badge">{profiles.length}</span></h1><p>Run your models. Keep everything close.</p></div><div className="button-row"><button onClick={onLibrary}><Plus size={15} /> Add model</button><button className="primary" aria-label={apiRunning ? 'Start selected' : 'Start server'} title={apiRunning && !selectedToStart.length ? selected.length ? 'All selected models are already running or changing state.' : 'Select a stopped model to start.' : undefined} disabled={apiRunning && !selectedToStart.length || startBusy || busy.includes('global-stop') || !!pendingStart} onClick={() => void start({ all: true }, 'all')}>{busy.includes('all') ? <Spinner /> : <Play size={14} fill="currentColor" />}{apiRunning ? 'Start selected' : 'Start server'}</button><button className="danger-button global-stop" aria-label="Stop server" title="Stop the inference API and all model workers. Studio controls stay available." disabled={!canStopServer || busy.includes('global-stop')} onClick={() => void stopServer()}>{busy.includes('global-stop') ? <Spinner size={14} /> : <Square size={13} />} {busy.includes('global-stop') ? 'Stopping…' : 'Stop server'}</button></div></div>
      <div className="api-banner"><div className="api-symbol"><ServerIcon size={20} /></div><div><div className="api-label">OpenAI-compatible endpoint <span className={`tag ${api.running ? 'green' : ''}`}>{api.running ? 'READY' : 'OFFLINE'}</span></div><div className="endpoint"><>{endpoints.length > 1 ? <select aria-label="API endpoint" className="endpoint-select" value={address} onChange={event => setChosenEndpoint(event.target.value)}>{endpoints.map(url => <option key={url} value={url}>{url}</option>)}</select> : <code>{address}</code>}</><CopyButton value={address} /></div></div><div className="api-banner-right"><span>{active.length} active model{active.length === 1 ? '' : 's'}</span><small>One endpoint, every model.</small></div></div>
      {api.error && <div className="inline-error" role="alert">Inference server could not start: {api.error}</div>}
      <div className="metrics-grid"><Metric icon={<MemoryStick size={16} />} label="SYSTEM MEMORY" value={system.ram_used_bytes == null ? '—' : bytes(system.ram_used_bytes)} detail={system.ram_total_bytes ? `of ${bytes(system.ram_total_bytes)} unified memory` : 'Waiting for telemetry'} progress={gauge} /><Metric icon={<Cpu size={16} />} label="CPU" value={system.cpu_percent == null ? '—' : `${Number(system.cpu_percent).toFixed(1)}%`} detail={system.cpu_cores ? `${system.cpu_cores} logical cores` : 'System utilization'} progress={system.cpu_percent ?? 0} /><Metric icon={<Activity size={16} />} label="MODEL MEMORY" value={totals.mlx_active_bytes ? bytes(totals.mlx_active_bytes) : '—'} detail={totals.rss_bytes ? `${bytes(totals.rss_bytes)} process RSS` : 'MLX active allocations'} /><Metric icon={<HardDrive size={16} />} label="SWAP" value={system.swap_used_bytes == null ? '—' : bytes(system.swap_used_bytes)} detail="System swap in use" progress={system.swap_total_bytes ? system.swap_used_bytes / system.swap_total_bytes * 100 : 0} /></div>
      <div className="section-toolbar server-selection-toolbar"><h2>Your servers</h2><span className="subtle">{selected.length} selected</span><div className="server-profile-search"><Search size={14} aria-hidden="true" /><input type="search" aria-label="Search local servers" placeholder="Search models or profiles…" value={searchQuery} onChange={event => changeSearch(event.target.value)} />{searchQuery && <button className="icon-button" aria-label="Clear server search" title="Clear search" onClick={() => changeSearch('')}><X size={13} /></button>}</div><div className="server-selection-controls"><span className="subtle server-visible-count" role="status">{visibleProfiles.length} of {profiles.length} {profiles.length === 1 ? 'model' : 'models'}</span><label className="server-selected-filter" title="Show only models checked for global start"><input type="checkbox" aria-label="Show selected models only" checked={selectedOnly} onChange={event => changeSelectedOnly(event.target.checked)} />Selected only</label></div></div>
      {!profiles.length ? <Empty icon={<ServerIcon size={28} />} title="Make room for your first model" action={<button className="primary" onClick={onLibrary}><Plus size={15} /> Open model library</button>}>Add a downloaded MLX model and create a server profile to get started.</Empty> : selectedOnly && !selected.length ? <Empty icon={<ServerIcon size={28} />} title="No selected models" action={<button onClick={() => changeSelectedOnly(false)}>Show all models</button>}>Show all models to choose which ones to serve.</Empty> : !visibleProfiles.length ? <Empty icon={<Search size={28} />} title="No matching servers" action={<button onClick={() => changeSearch('')}>Clear search</button>}>{selectedOnly ? 'No selected model matches this search. Try another profile, model name, or repository.' : 'Try another profile, model name, or repository.'}</Empty> : <div className="server-table"><div className="server-table-heading"><span /><span>MODEL / PROFILE</span><span>STATUS / ACTIVITY</span><span>MEMORY</span><span>CPU</span><span>REQUESTS</span><span /></div>{visibleProfiles.map(profile => {
        const server = snapshot.servers.find(item => item.profile_id === profile.id);
        const worker = (telemetry.workers ?? []).find((item: JsonObject) => matchesWorker(item, server));
        const activity = readActivity(worker, server);
        const model = snapshot.models.find(item => item.id === profile.model_id);
        const isStopping = !!server && ['draining', 'stopping'].includes(server.state);
        const isActive = !!server && OCCUPIED_STATES.includes(server.state);
        return <div className={`server-row ${profile.selected ? 'selected' : ''} ${logId === profile.id ? 'focused' : ''}`} key={profile.id} role="group" aria-label={`Server ${profile.alias}`}><label className="checkbox-wrap"><input type="checkbox" aria-label={`Select ${profile.alias}`} checked={profile.selected} onChange={event => void execute('profiles.save', { profile: { ...profile, selected: event.target.checked } }, `select:${profile.id}`)} /><span /></label><button className="server-name" onClick={() => { setLogId(profile.id); setLogsOpen(true); }}><ModelLogo repoId={model?.repo_id} name={model?.name ?? profile.alias} size={34} className={`model-icon ${isActive ? 'running' : ''}`} /><span><strong>{profile.alias}</strong><small>{model?.repo_id ?? model?.name ?? profile.model_id}</small><span className="server-tags"><span>{model?.quantization ?? 'MLX'}</span>{server?.port && <span>:{server.port}</span>}{server?.pid && <span>PID {server.pid}</span>}</span></span></button><div className="server-status-activity"><Status state={server?.state ?? 'stopped'} /><div className="server-activity" aria-label={`Activity for ${profile.alias}`} data-phase={activity.phase}>{(server?.state ?? 'stopped') !== 'stopped' && <span className="server-activity-phase" title={activity.description}>{activity.label}</span>}{activity.mixed && <small className="server-activity-mixed">{activity.mixed}</small>}<small className="server-throughput" title="Measured output tokens per second over the latest valid sampling interval. A dash means no current measurement.">{activity.rate == null ? '— tok/s' : `${activity.rate.toFixed(1)} tok/s`}</small></div>{server?.error && <small className="server-error" title={server.error}>{server.error}</small>}</div><div className="table-value">{worker?.rss_bytes == null ? '—' : bytes(worker.rss_bytes)}<small>{worker?.mlx_active_bytes != null ? `${bytes(worker.mlx_active_bytes)} MLX` : model ? `${bytes(model.weights_bytes)} weights` : ''}</small></div><div className="table-value">{worker?.cpu_percent == null ? '—' : `${Number(worker.cpu_percent).toFixed(1)}%`}<small>process</small></div><div className="table-value server-request-count" aria-label={`Requests for ${profile.alias}`}><span>{activity.active == null ? '— active' : `${activity.active.toLocaleString()} active`}</span><small title="Requests reported by this worker since its current process started.">{activity.total == null ? '— total' : `${activity.total.toLocaleString()} total`}</small></div><div className="server-row-actions"><button title="View server logs" aria-label={`Logs for ${profile.alias}`} className="icon-button" onClick={() => { setLogId(profile.id); setLogsOpen(true); }}><Terminal size={16} /></button><button className={isActive ? 'stop-server' : 'start-server'} disabled={busy.includes('global-stop') || isStopping || busy.includes(`${isActive ? 'stop' : 'start'}:${profile.id}`) || !isActive && (startBusy || !!pendingStart)} onClick={() => isActive ? void execute('server.stop', { id: profile.id }, `stop:${profile.id}`) : void start({ id: profile.id }, `start:${profile.id}`)}>{busy.includes(`${isActive ? 'stop' : 'start'}:${profile.id}`) ? <Spinner size={14} /> : isActive ? <Square size={12} /> : <Play size={12} fill="currentColor" />}{isStopping ? 'Stopping…' : isActive ? 'Stop' : 'Start'}</button></div></div>;
      })}</div>}
      {logId && <section className="worker-memory-panel" aria-label="Selected model memory">
        <header><div><span className="section-label">SELECTED MODEL MEMORY</span><h2>{displayModelName(selectedModel?.name)}</h2><p>Profile <code>{selectedProfile?.alias ?? '—'}</code></p></div><div className="worker-weights"><span>Weights on disk</span><strong>{selectedModel?.weights_bytes ? bytes(selectedModel.weights_bytes) : '—'}</strong></div></header>
        <p className="worker-memory-context">Showing the profile selected for Server logs. Choose a model row or the log selector to inspect another model.</p>
        <dl className="worker-memory-grid"><MemoryReading label="MLX active" value={selectedMemory?.mlx_active_bytes} description="Allocations currently used by MLX." /><MemoryReading label="MLX cache" value={selectedMemory?.mlx_cache_bytes} description="Allocated memory kept by MLX for reuse." /><MemoryReading label="Process RSS" value={selectedMemory?.rss_bytes} description="Current physical memory reported by macOS." /><MemoryReading label="Estimated releasable" value={potentiallyReleasable(selectedMemory)} description="Estimate on stopping this worker; not guaranteed." /></dl>
        <details className="worker-memory-peaks"><summary>Peaks since worker start</summary><dl><MemoryReading label="MLX peak" value={selectedMemory?.mlx_peak_bytes} description="Highest MLX allocation reported by this worker." /><MemoryReading label="Locals RSS peak" value={selectedMemory?.rss_peak_bytes} description="Peak resident memory reported by Locals." /></dl></details>
        <p className="worker-memory-note"><CircleHelp size={14} /> Apple Silicon uses unified memory. RSS and MLX overlap; they are never added together. Estimated releasable memory is max(process RSS, MLX active + cache).</p>
      </section>}
      <div className="memory-note"><CircleHelp size={14} /><span>GPU utilization is unavailable. The total below sums each worker's releasable-memory estimate.</span></div>
      <div className="estimate-strip"><span><MemoryStick size={14} /> Est. potentially releasable · all workers <strong>{totalReleasable == null ? '—' : bytes(totalReleasable)}</strong></span></div>
      {lastEstimate && <div className="estimate-strip"><span><MemoryStick size={14} /> Selected + running model estimate <strong>{bytes(lastEstimate.total_estimated_bytes)}</strong></span><span>Budget {bytes(lastEstimate.budget_bytes)} <span className="tag">{lastEstimate.complete ? 'ESTIMATE' : 'PARTIAL'}</span></span></div>}
      {error && <div className="inline-error">{error}</div>}
    </div>
    <section className={`logs-panel ${!logsOpen ? 'collapsed' : ''}`} style={{ height: logsOpen ? logHeight : 40 }}>{logsOpen && <div className="log-resize-handle" role="separator" aria-label="Resize server logs" aria-orientation="horizontal" aria-valuemin={100} aria-valuemax={480} aria-valuenow={logHeight} tabIndex={0} onKeyDown={event => { if (event.key === 'ArrowUp' || event.key === 'ArrowDown') { event.preventDefault(); setLogHeight(height => Math.max(100, Math.min(480, height + (event.key === 'ArrowUp' ? 20 : -20)))); } }} onPointerDown={event => { logDrag.current = { y: event.clientY, height: logHeight }; event.currentTarget.setPointerCapture(event.pointerId); }} onPointerMove={event => { if (logDrag.current) setLogHeight(Math.max(100, Math.min(Math.min(480, window.innerHeight * .65), logDrag.current.height + logDrag.current.y - event.clientY))); }} onPointerUp={event => { logDrag.current = null; event.currentTarget.releasePointerCapture(event.pointerId); }} />}<div className="logs-heading"><button className="logs-toggle" onClick={() => setLogsOpen(!logsOpen)}><Terminal size={14} /><strong>Server logs</strong><ChevronDown size={13} /></button><div><select aria-label="Server logs profile" value={logId} onChange={event => setLogId(event.target.value)}><option value="">Select a server</option>{profiles.map(profile => <option key={profile.id} value={profile.id}>{profile.alias}</option>)}</select><CopyButton value={logs.join('\n')} /><button className="icon-button" aria-label={logsOpen ? 'Collapse logs' : 'Expand logs'} onClick={() => setLogsOpen(!logsOpen)}>{logsOpen ? <X size={14} /> : <Terminal size={14} />}</button></div></div>{logsOpen && <div className="log-lines" role="log" aria-label="Server logs">{logs.length ? logs.map((line, index) => <div key={index} className={/error|traceback|failed/i.test(line) ? 'error' : /ready|listening|started/i.test(line) ? 'success' : ''}><span>{String(index + 1).padStart(3, '0')}</span>{line}</div>) : <p>Server output will appear here when a model starts.</p>}</div>}</section>
    {estimate && pendingStart && <Modal title="Review memory usage" onClose={() => { setEstimate(null); setPendingStart(null); }}><div className="settings-body"><div className="memory-review"><MemoryStick size={24} /><strong>{bytes(estimate.total_estimated_bytes)}</strong><span>estimated · {bytes(estimate.budget_bytes)} budget</span></div>{estimate.warnings?.map((warning: JsonObject) => <p className="warning-message" key={warning.code}>{warning.message}</p>)}<p className="subtle">{estimate.note}</p></div><footer className="modal-footer"><button onClick={() => { setEstimate(null); setPendingStart(null); }}>Cancel</button><button className="danger-button" aria-label="Stop server and cancel memory review" onClick={() => void stopServer()}><Square size={12} /> Stop server</button><button className="primary" onClick={() => { const { key, epoch, ...params } = pendingStart; setEstimate(null); setPendingStart(null); if (epoch === startEpoch.current) void execute('server.start', { ...params, allow_memory_overcommit: true }, key); }}>Start anyway</button></footer></Modal>}
  </div>;
}

function Metric({ icon, label, value, detail, progress }: { icon: React.ReactNode; label: string; value: string; detail: string; progress?: number }) {
  return <div className="metric"><div className="metric-label">{icon}<span>{label}</span></div><strong>{value}</strong><small>{detail}</small>{progress != null && <div className="metric-track"><span style={{ width: `${Math.min(100, Math.max(0, progress))}%` }} /></div>}</div>;
}

function MemoryReading({ label, value, description }: { label: string; value: number | null | undefined; description: string }) {
  return <div className="worker-memory-reading"><dt title={description}>{label}</dt><dd>{value == null ? '—' : bytes(value)}</dd><dd className="worker-memory-description">{description}</dd></div>;
}

function displayModelName(name: string | undefined) {
  if (!name) return '—';
  // Presentation only: repository IDs and model paths keep their original spelling.
  return name.replace(/^qwen(?=\d)/i, 'Qwen').replace(/(\d)b(?=[-_]|$)/gi, '$1B').replace(/(^|[-_])mlx(?=[-_]|$)/gi, '$1MLX');
}

function potentiallyReleasable(worker: JsonObject | undefined): number | null {
  if (!worker) return null;
  const { rss_bytes, mlx_active_bytes, mlx_cache_bytes } = worker;
  if (![rss_bytes, mlx_active_bytes, mlx_cache_bytes].every(value => typeof value === 'number' && Number.isFinite(value) && value >= 0)) return null;
  return Math.max(rss_bytes, mlx_active_bytes + mlx_cache_bytes);
}


function matchesWorker(worker: JsonObject | undefined, server: Server | undefined): boolean {
  return server?.pid != null && worker?.profile_id === server.profile_id && worker.pid === server.pid
    && (worker.started_at == null || server.started_at == null || worker.started_at === server.started_at)
    && (worker.port == null || server.port == null || worker.port === server.port);
}

function count(value: unknown): number | null {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : null;
}

function readActivity(worker: JsonObject | undefined, server: Server | undefined) {
  const state = server?.state ?? 'stopped';
  const reading = { phase: 'unknown', label: 'Unknown', description: 'Live worker activity is unavailable.', rate: null as number | null, active: count(server?.active_requests), total: null as number | null, mixed: '' };
  if (!OCCUPIED_STATES.includes(state)) return { ...reading, phase: 'stopped', label: state === 'stopped' ? 'Stopped' : 'Not running', description: 'This model worker is not running.', active: 0 };
  if (['starting', 'loading'].includes(state)) return { ...reading, phase: 'loading', label: 'Loading model', description: 'The model worker is starting or loading its weights.' };
  if (['draining', 'stopping'].includes(state)) return { ...reading, phase: 'stopping', label: 'Stopping', description: 'The model worker is draining or stopping.' };
  const activity = worker?.activity;
  if (!activity || typeof activity !== 'object' || Array.isArray(activity)) return reading;
  const phases: Record<string, { label: string; description: string }> = {
    idle: { label: 'Idle', description: 'No active request is reported by this worker.' },
    preparing: { label: 'Prompt', description: 'Queued or preparing the prompt; waiting for the first output. This does not identify an exact GPU processing stage.' },
    thinking: { label: 'Thinking', description: 'The response stream currently contains reasoning output.' },
    generating: { label: 'Generating', description: 'The response stream currently contains answer text or tool output.' },
    decoding: { label: 'Decoding', description: 'The worker is producing tokens; reasoning versus answer output is unavailable.' },
    mixed: { label: 'Mixed activity', description: 'Concurrent requests are in different observed stages.' },
    unknown: { label: 'Unknown', description: 'The current request stage is unavailable.' },
  };
  const phase = typeof activity.phase === 'string' && phases[activity.phase] ? activity.phase : 'unknown';
  const active = count(activity.active_requests) ?? reading.active;
  const rate = ['thinking', 'generating', 'decoding', 'mixed'].includes(phase) && activity.stats_available === true && active != null && active > 0 && typeof activity.tokens_per_second === 'number' && Number.isFinite(activity.tokens_per_second) && activity.tokens_per_second >= 0 ? activity.tokens_per_second : null;
  const mixed = phase === 'mixed' && activity.phases && typeof activity.phases === 'object' && !Array.isArray(activity.phases)
    ? ['preparing', 'thinking', 'generating', 'decoding', 'unknown'].filter(key => (count(activity.phases[key]) ?? 0) > 0).map(key => `${activity.phases[key]} ${key === 'preparing' ? 'prompt' : key}`).join(' · ')
    : '';
  return { phase, ...phases[phase], rate, active, total: count(activity.total_requests) ?? count(worker?.stats?.requests), mixed };
}
