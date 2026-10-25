import { useEffect, useRef, useState } from 'react';
import { Activity, ArrowUpRight, Check, ChevronDown, CircleHelp, Cpu, Download, HardDrive, MemoryStick, Play, Plus, RefreshCw, Server as ServerIcon, Square, Terminal, X } from 'lucide-react';
import type { JsonObject, Profile, Snapshot } from '../types';
import { bytes, logLines } from '../utils.mjs';
import { CopyButton, Empty, Modal, Spinner, Status } from '../components';
import { errorMessage, request } from '../api';

interface Props { snapshot: Snapshot; onAction: (method: string, params?: JsonObject) => Promise<any>; onLibrary: () => void; }
export default function ServerView({ snapshot, onAction, onLibrary }: Props) {
  const [busy, setBusy] = useState<string[]>([]);
  const [logId, setLogId] = useState('');
  const [logs, setLogs] = useState<string[]>([]);
  const [logsOpen, setLogsOpen] = useState(true);
  const [logHeight, setLogHeight] = useState(185);
  const logDrag = useRef<{ y: number; height: number } | null>(null);
  const requests = useRef(new Set<string>());
  const [error, setError] = useState('');
  const [estimate, setEstimate] = useState<JsonObject | null>(null);
  const [pendingStart, setPendingStart] = useState<JsonObject | null>(null);
  const [lastEstimate, setLastEstimate] = useState<JsonObject | null>(null);
  const telemetry = snapshot.telemetry;
  const system = telemetry.system ?? {};
  const totals = telemetry.managed_totals ?? {};
  const active = snapshot.servers.filter(server => ['running', 'ready'].includes(server.state));
  const selected = snapshot.profiles.filter(profile => profile.selected);
  const selectedWorker = (telemetry.workers ?? []).find((worker: JsonObject) => worker.profile_id === logId);
  const selectedAlias = snapshot.profiles.find(profile => profile.id === logId)?.alias ?? 'Selected worker';
  const releaseEstimates = (telemetry.workers ?? []).filter((worker: JsonObject) => worker.pid != null).map(potentiallyReleasable);
  const missingWorkerTelemetry = snapshot.servers.some(server => server.pid != null && !(telemetry.workers ?? []).some((worker: JsonObject) => worker.profile_id === server.profile_id && worker.pid === server.pid));
  const totalReleasable = !missingWorkerTelemetry && releaseEstimates.every((value: number | null) => value != null) ? releaseEstimates.reduce((sum: number, value: number) => sum + value, 0) : null;
  const workerStateKey = snapshot.servers.map(server => `${server.profile_id}:${server.pid ?? ''}:${server.state}`).sort().join('|');
  const api = snapshot.api ?? {};
  const [chosenEndpoint, setChosenEndpoint] = useState('');
  const endpoints: string[] = api.urls?.length ? api.urls : [`http://${snapshot.config.api?.host ?? '127.0.0.1'}:${snapshot.config.api?.port ?? 8321}/v1`];
  const address = endpoints.includes(chosenEndpoint) ? chosenEndpoint : endpoints[0];
  useEffect(() => { if (!snapshot.profiles.some(profile => profile.id === logId)) setLogId(snapshot.profiles[0]?.id ?? ''); }, [snapshot.profiles, logId]);
  useEffect(() => {
    let alive = true;
    setLogs([]);
    const read = async () => { if (!logId || !logsOpen) return; try { const result = await request('server.logs', { id: logId }); if (alive) setLogs([...(result.truncated ? ['Earlier log lines omitted. Showing the latest output.'] : []), ...logLines(result)]); } catch (failure) { if (alive) setLogs([`Could not read server logs: ${errorMessage(failure)}`]); } };
    void read(); const timer = window.setInterval(() => void read(), 3000);
    return () => { alive = false; window.clearInterval(timer); };
  }, [logId, logsOpen, onAction]);
  useEffect(() => { let alive = true; setLastEstimate(null); void request('memory.estimate').then(value => { if (alive) setLastEstimate(value); }).catch(() => {}); return () => { alive = false; }; }, [snapshot.profiles, snapshot.models, workerStateKey]);

  const execute = async (method: string, params: JsonObject, key: string) => {
    if (requests.current.has(key)) return;
    requests.current.add(key);
    setError(''); setBusy(previous => [...previous, key]);
    try { await onAction(method, params); }
    catch (failure) { setError(errorMessage(failure)); }
    finally { requests.current.delete(key); setBusy(previous => previous.filter(item => item !== key)); }
  };
  const start = async (params: JsonObject, key: string) => {
    if (requests.current.has(key) || pendingStart) return;
    requests.current.add(key);
    setError(''); setBusy(previous => [...previous, key]);
    try {
      const result = await request('memory.estimate', params);
      if (result.requires_override) { setEstimate(result); setPendingStart({ ...params, key }); return; }
      await onAction('server.start', params);
    } catch (failure) { setError(errorMessage(failure)); }
    finally { requests.current.delete(key); setBusy(previous => previous.filter(item => item !== key)); }
  };
  const gauge = system.ram_total_bytes ? Math.min(100, system.ram_used_bytes / system.ram_total_bytes * 100) : 0;

  return <div className="server-view">
    <div className="page-content server-content"><div className="page-title"><div><div className="eyebrow">LOCAL INFERENCE</div><h1>Model server <span className="count-badge">{snapshot.profiles.length}</span></h1><p>Run your models. Keep everything close.</p></div><div className="button-row"><button onClick={onLibrary}><Plus size={15} /> Add model</button><button className="primary" disabled={!selected.length || busy.includes('all') || !!pendingStart} onClick={() => void start({ all: true }, 'all')}>{busy.includes('all') ? <Spinner /> : <Play size={14} fill="currentColor" />} Start selected</button></div></div>
      <div className="api-banner"><div className="api-symbol"><ServerIcon size={20} /></div><div><div className="api-label">OpenAI-compatible endpoint <span className={`tag ${api.running ? 'green' : ''}`}>{api.running ? 'READY' : 'OFFLINE'}</span></div><div className="endpoint"><>{endpoints.length > 1 ? <select aria-label="API endpoint" className="endpoint-select" value={address} onChange={event => setChosenEndpoint(event.target.value)}>{endpoints.map(url => <option key={url} value={url}>{url}</option>)}</select> : <code>{address}</code>}</><CopyButton value={address} /></div></div><div className="api-banner-right"><span>{active.length} active model{active.length === 1 ? '' : 's'}</span><small>One endpoint, every model.</small></div></div>
      <div className="metrics-grid"><Metric icon={<MemoryStick size={16} />} label="SYSTEM MEMORY" value={system.ram_used_bytes == null ? '—' : bytes(system.ram_used_bytes)} detail={system.ram_total_bytes ? `of ${bytes(system.ram_total_bytes)} unified memory` : 'Waiting for telemetry'} progress={gauge} /><Metric icon={<Cpu size={16} />} label="CPU" value={system.cpu_percent == null ? '—' : `${Number(system.cpu_percent).toFixed(1)}%`} detail={system.cpu_cores ? `${system.cpu_cores} logical cores` : 'System utilization'} progress={system.cpu_percent ?? 0} /><Metric icon={<Activity size={16} />} label="MODEL MEMORY" value={totals.mlx_active_bytes ? bytes(totals.mlx_active_bytes) : '—'} detail={totals.rss_bytes ? `${bytes(totals.rss_bytes)} process RSS` : 'MLX active allocations'} /><Metric icon={<HardDrive size={16} />} label="SWAP" value={system.swap_used_bytes == null ? '—' : bytes(system.swap_used_bytes)} detail="System swap in use" progress={system.swap_total_bytes ? system.swap_used_bytes / system.swap_total_bytes * 100 : 0} /></div>
      <div className="section-toolbar"><h2>Your servers</h2><span className="subtle">{selected.length} selected <span className="dot-separator">·</span> Each model runs independently</span><button className="text-button" disabled={!active.length || busy.includes('stop-all')} onClick={() => void execute('server.stop', { all: true }, 'stop-all')}><Square size={12} /> Stop all</button></div>
      {!snapshot.profiles.length ? <Empty icon={<ServerIcon size={28} />} title="Make room for your first model" action={<button className="primary" onClick={onLibrary}><Plus size={15} /> Open model library</button>}>Add a downloaded MLX model and create a server profile to get started.</Empty> : <div className="server-table"><div className="server-table-heading"><span /><span>MODEL / PROFILE</span><span>STATUS</span><span>MEMORY</span><span>CPU</span><span>REQUESTS</span><span /></div>{snapshot.profiles.map(profile => {
        const server = snapshot.servers.find(item => item.profile_id === profile.id);
        const worker = (telemetry.workers ?? []).find((item: JsonObject) => item.profile_id === profile.id);
        const model = snapshot.models.find(item => item.id === profile.model_id);
        const isActive = server && ['running', 'ready', 'starting', 'loading'].includes(server.state);
        return <div className={`server-row ${logId === profile.id ? 'focused' : ''}`} key={profile.id}><label className="checkbox-wrap"><input type="checkbox" aria-label={`Select ${profile.alias}`} checked={profile.selected} onChange={event => void execute('profiles.save', { profile: { ...profile, selected: event.target.checked } }, `select:${profile.id}`)} /><span /></label><button className="server-name" onClick={() => { setLogId(profile.id); setLogsOpen(true); }}><span className={`model-icon ${isActive ? 'running' : ''}`}><Cpu size={18} /></span><span><strong>{profile.alias}</strong><small>{model?.repo_id ?? model?.name ?? profile.model_id}</small><span className="server-tags"><span>{model?.quantization ?? 'MLX'}</span>{server?.port && <span>:{server.port}</span>}{server?.pid && <span>PID {server.pid}</span>}</span></span></button><div><Status state={server?.state ?? 'stopped'} />{server?.error && <small className="server-error" title={server.error}>{server.error}</small>}</div><div className="table-value">{worker?.rss_bytes == null ? '—' : bytes(worker.rss_bytes)}<small>{worker?.mlx_active_bytes != null ? `${bytes(worker.mlx_active_bytes)} MLX` : model ? `${bytes(model.weights_bytes)} weights` : ''}</small></div><div className="table-value">{worker?.cpu_percent == null ? '—' : `${Number(worker.cpu_percent).toFixed(1)}%`}<small>process</small></div><div className="table-value">{server?.active_requests ?? 0}<small>active</small></div><div className="server-row-actions"><button title="View server logs" aria-label={`Logs for ${profile.alias}`} className="icon-button" onClick={() => { setLogId(profile.id); setLogsOpen(true); }}><Terminal size={16} /></button><button className={isActive ? 'stop-server' : 'start-server'} disabled={busy.includes(`${isActive ? 'stop' : 'start'}:${profile.id}`) || !isActive && !!pendingStart} onClick={() => isActive ? void execute('server.stop', { id: profile.id }, `stop:${profile.id}`) : void start({ id: profile.id }, `start:${profile.id}`)}>{busy.includes(`${isActive ? 'stop' : 'start'}:${profile.id}`) ? <Spinner size={14} /> : isActive ? <Square size={12} /> : <Play size={12} fill="currentColor" />}{isActive ? 'Stop' : 'Start'}</button></div></div>;
      })}</div>}
      <div className="memory-note"><CircleHelp size={14} /><span>Apple Silicon uses unified memory. Potentially releasable memory is estimated as max(current RSS, MLX active + cache) per worker, then summed across workers. RSS and MLX are never added together. GPU utilization is unavailable.</span></div>
      {logId && <section className="estimate-strip" aria-label="Selected worker memory details" style={{ flexWrap: 'wrap', gap: '10px 18px' }}><span><Activity size={14} /><strong>{selectedAlias}</strong></span>{[['MLX active', selectedWorker?.mlx_active_bytes], ['MLX cache', selectedWorker?.mlx_cache_bytes], ['MLX peak', selectedWorker?.mlx_peak_bytes], ['Locals RSS peak', selectedWorker?.rss_peak_bytes], ['Est. releasable', potentiallyReleasable(selectedWorker)]].map(([label, value]) => <span key={String(label)}>{label}<strong>{value == null ? '—' : bytes(Number(value))}</strong></span>)}</section>}
      <div className="estimate-strip"><span><MemoryStick size={14} /> Est. potentially releasable · all workers <strong>{totalReleasable == null ? '—' : bytes(totalReleasable)}</strong></span></div>
      {lastEstimate && <div className="estimate-strip"><span><MemoryStick size={14} /> Selected + running model estimate <strong>{bytes(lastEstimate.total_estimated_bytes)}</strong></span><span>Budget {bytes(lastEstimate.budget_bytes)} <span className="tag">{lastEstimate.complete ? 'ESTIMATE' : 'PARTIAL'}</span></span></div>}
      {error && <div className="inline-error">{error}</div>}
    </div>
    <section className={`logs-panel ${!logsOpen ? 'collapsed' : ''}`} style={{ height: logsOpen ? logHeight : 40 }}>{logsOpen && <div className="log-resize-handle" role="separator" aria-label="Resize server logs" aria-orientation="horizontal" aria-valuemin={100} aria-valuemax={480} aria-valuenow={logHeight} tabIndex={0} onKeyDown={event => { if (event.key === 'ArrowUp' || event.key === 'ArrowDown') { event.preventDefault(); setLogHeight(height => Math.max(100, Math.min(480, height + (event.key === 'ArrowUp' ? 20 : -20)))); } }} onPointerDown={event => { logDrag.current = { y: event.clientY, height: logHeight }; event.currentTarget.setPointerCapture(event.pointerId); }} onPointerMove={event => { if (logDrag.current) setLogHeight(Math.max(100, Math.min(Math.min(480, window.innerHeight * .65), logDrag.current.height + logDrag.current.y - event.clientY))); }} onPointerUp={event => { logDrag.current = null; event.currentTarget.releasePointerCapture(event.pointerId); }} />}<div className="logs-heading"><button className="logs-toggle" onClick={() => setLogsOpen(!logsOpen)}><Terminal size={14} /><strong>Server logs</strong><ChevronDown size={13} /></button><div><select aria-label="Server logs profile" value={logId} onChange={event => setLogId(event.target.value)}><option value="">Select a server</option>{snapshot.profiles.map(profile => <option key={profile.id} value={profile.id}>{profile.alias}</option>)}</select><CopyButton value={logs.join('\n')} /><button className="icon-button" aria-label={logsOpen ? 'Collapse logs' : 'Expand logs'} onClick={() => setLogsOpen(!logsOpen)}>{logsOpen ? <X size={14} /> : <Terminal size={14} />}</button></div></div>{logsOpen && <div className="log-lines" role="log" aria-label="Server logs">{logs.length ? logs.map((line, index) => <div key={index} className={/error|traceback|failed/i.test(line) ? 'error' : /ready|listening|started/i.test(line) ? 'success' : ''}><span>{String(index + 1).padStart(3, '0')}</span>{line}</div>) : <p>Server output will appear here when a model starts.</p>}</div>}</section>
    {estimate && pendingStart && <Modal title="Review memory usage" onClose={() => { setEstimate(null); setPendingStart(null); }}><div className="settings-body"><div className="memory-review"><MemoryStick size={24} /><strong>{bytes(estimate.total_estimated_bytes)}</strong><span>estimated · {bytes(estimate.budget_bytes)} budget</span></div>{estimate.warnings?.map((warning: JsonObject) => <p className="warning-message" key={warning.code}>{warning.message}</p>)}<p className="subtle">{estimate.note}</p></div><footer className="modal-footer"><button onClick={() => { setEstimate(null); setPendingStart(null); }}>Cancel</button><button className="primary" onClick={() => { const { key, ...params } = pendingStart; setEstimate(null); setPendingStart(null); void execute('server.start', { ...params, allow_memory_overcommit: true }, key); }}>Start anyway</button></footer></Modal>}
  </div>;
}

function Metric({ icon, label, value, detail, progress }: { icon: React.ReactNode; label: string; value: string; detail: string; progress?: number }) {
  return <div className="metric"><div className="metric-label">{icon}<span>{label}</span></div><strong>{value}</strong><small>{detail}</small>{progress != null && <div className="metric-track"><span style={{ width: `${Math.min(100, Math.max(0, progress))}%` }} /></div>}</div>;
}

function potentiallyReleasable(worker: JsonObject | undefined): number | null {
  if (!worker) return null;
  const { rss_bytes, mlx_active_bytes, mlx_cache_bytes } = worker;
  if (![rss_bytes, mlx_active_bytes, mlx_cache_bytes].every(value => typeof value === 'number' && Number.isFinite(value) && value >= 0)) return null;
  return Math.max(rss_bytes, mlx_active_bytes + mlx_cache_bytes);
}
