import { useEffect, useState } from 'react';
import KapitsaLogo from '../KapitsaLogo';
import { ArrowDownToLine, Boxes, CheckCircle2, History, RefreshCw, RotateCcw } from 'lucide-react';
import { ENGINES, ENGINE_LABELS, type Engine, type EngineProgress, type EngineStatus, type JsonObject, type Snapshot } from '../types';
import { Spinner, Status } from '../components';
import { engineOf, restartRequired } from '../engines';
import { errorMessage } from '../api';
import { bytes } from '../utils.mjs';

const ABOUT: Record<Engine, { summary: string; source: string }> = {
  locals: {
    summary: 'Many requests at once on MLX, with a prompt cache and tool calls and JSON that always parse. OpenAI- and Ollama-compatible.',
    source: 'Each version gets its own Python environment, built with uv from the Locals release and the dependencies it was tested with.',
  },
  kapitsa: {
    summary: 'Fast decoding of Qwen, Nemotron, Flint and more on Apple silicon, with draft models and an SSD prompt cache. OpenAI- and Anthropic-compatible.',
    source: 'Each version is the release published on GitHub, checked against its SHA-256. Kapitsa never updates itself here.',
  },
};

const PHASES: Record<string, string> = {
  downloading: 'Downloading', installing_python: 'Installing Python', installing_locals: 'Installing Locals',
  installing: 'Installing', verifying: 'Verifying', ready: 'Installed', up_to_date: 'Already up to date', rolled_back: 'Previous version restored',
};

const busyPhase = (phase?: string) => !!phase && !['ready', 'up_to_date', 'rolled_back'].includes(phase);

function checkedAgo(at?: string | null) {
  if (!at) return '';
  const minutes = Math.round((Date.now() - Date.parse(at)) / 60000);
  return minutes < 1 ? 'just now' : minutes < 60 ? `${minutes} min ago` : new Date(at).toLocaleString();
}

function EngineCard({ engine, status, progress, snapshot, onAction }: { engine: Engine; status?: EngineStatus; progress?: EngineProgress; snapshot: Snapshot; onAction: (method: string, params?: JsonObject) => Promise<any> }) {
  const [busy, setBusy] = useState('');
  const [error, setError] = useState('');
  const servers = snapshot.servers.filter(server => engineOf(server) === engine && server.pid != null);
  const working = busy !== '' || busyPhase(progress?.phase);
  const latest = status?.latest;
  const act = async (method: string, extra: JsonObject = {}) => {
    setBusy(method); setError('');
    try { await onAction(method, { engine, ...extra }); }
    catch (failure) { setError(errorMessage(failure)); }
    finally { setBusy(''); }
  };
  const restart = async (profileId: string) => {
    setBusy(`restart:${profileId}`); setError('');
    try { await onAction('server.stop', { id: profileId }); await onAction('server.start', { id: profileId }); }
    catch (failure) { setError(errorMessage(failure)); }
    finally { setBusy(''); }
  };
  const failure = error || progress?.error;
  const share = progress?.total_bytes ? Math.min(100, Math.round(((progress.received_bytes ?? 0) / progress.total_bytes) * 100)) : null;
  return <section className="engine-card" aria-label={`${ENGINE_LABELS[engine]} engine`}>
    <header className="engine-card-head">
      <span className={`engine-card-icon engine-card-icon-${engine}`}>{engine === 'kapitsa' ? <KapitsaLogo size={30} /> : <Boxes size={20} />}</span>
      <div className="engine-card-title">
        <h2>{ENGINE_LABELS[engine]}</h2>
        <p>{ABOUT[engine].summary}</p>
      </div>
      <span className={`tag ${status?.installed ? 'green' : ''}`}>{status?.installed ? `Installed · ${status.version}` : status ? 'Not installed' : 'Unknown'}</span>
    </header>
    <dl className="engine-facts">
      <div><dt>Installed</dt><dd>{status?.version ?? '—'}{status?.source === 'local' && <small> from a local checkout</small>}</dd></div>
      <div><dt>Latest release</dt><dd>{latest?.version ? <>{latest.version}{status?.update_available && <span className="tag warning">Update available</span>}{!status?.update_available && status?.installed && latest.version === status.version && <span className="engine-uptodate"><CheckCircle2 size={13} /> Up to date</span>}</> : latest?.error ? <span className="engine-error-text">Could not check</span> : 'Not checked yet'}{latest?.checked_at && <small> · checked {checkedAgo(latest.checked_at)}</small>}</dd></div>
      <div><dt>Previous</dt><dd>{status?.previous?.version ?? '—'}</dd></div>
      <div><dt>Minimum</dt><dd>{status?.minimum_version ?? '—'}</dd></div>
    </dl>
    {latest?.error && <p className="engine-error-text" role="alert">{latest.error}</p>}
    <div className="button-row engine-actions">
      {!status?.installed
        ? <button className="primary" disabled={working} onClick={() => void act('runtime.install')}>{busy === 'runtime.install' ? <Spinner size={15} /> : <ArrowDownToLine size={15} />} Install {ENGINE_LABELS[engine]}</button>
        : <button className={status.update_available ? 'primary' : ''} disabled={working} onClick={() => void act('runtime.update')}>{busy === 'runtime.update' ? <Spinner size={15} /> : <ArrowDownToLine size={15} />} {status.update_available ? `Update to ${latest?.version}` : 'Update'}</button>}
      <button disabled={working} onClick={() => void act('runtime.check', { force: true })}>{busy === 'runtime.check' ? <Spinner size={15} /> : <RefreshCw size={14} />} Check for updates</button>
      {status?.previous && <button disabled={working} onClick={() => void act('runtime.rollback')} title={`Make ${status.previous.version} current again. Running servers keep their version until they restart.`}>{busy === 'runtime.rollback' ? <Spinner size={15} /> : <History size={14} />} Return to {status.previous.version}</button>}
    </div>
    {(progress?.phase || (progress?.logs.length ?? 0) > 0) && <div className="runtime-progress engine-progress">
      <span>{working && <Spinner size={13} />} {PHASES[progress?.phase ?? ''] ?? progress?.phase?.replaceAll('_', ' ')}{progress?.version && ` ${progress.version}`}{share != null && ` · ${share}% of ${bytes(progress?.total_bytes ?? 0)}`}</span>
      {share != null && <div className="metric-track"><span style={{ width: `${share}%` }} /></div>}
      {(progress?.logs.length ?? 0) > 0 && <pre role="log" aria-label={`${ENGINE_LABELS[engine]} installation output`}>{progress!.logs.slice(-8).join('\n')}</pre>}
    </div>}
    {failure && <p className="engine-error-text" role="alert">{failure}</p>}
    <p className="subtle tiny">{ABOUT[engine].source} An update installs beside the current version: running servers keep theirs until they restart.</p>
    <div className="engine-servers">
      <div className="section-label">SERVERS ON {ENGINE_LABELS[engine].toUpperCase()}</div>
      {servers.length === 0 ? <p className="subtle tiny">No server runs on {ENGINE_LABELS[engine]}. Choose it for a profile in Model server or My models.</p> : servers.map(server => {
        const stale = restartRequired(server, snapshot);
        return <div key={server.profile_id} className="engine-server-row">
          <strong>{server.alias}</strong><Status state={server.state} />
          <small>{server.runtime_version ? `${ENGINE_LABELS[engine]} ${server.runtime_version}` : 'version unknown'}</small>
          {stale && <span className="tag warning" title={`Started on ${server.runtime_version}; ${status?.version} applies after a restart`}>Restart required</span>}
          {stale && <button className="text-button" disabled={working} onClick={() => void restart(server.profile_id)}>{busy === `restart:${server.profile_id}` ? <Spinner size={13} /> : <RotateCcw size={13} />} Restart</button>}
        </div>;
      })}
    </div>
  </section>;
}

export default function RuntimeView({ snapshot, progress, onAction }: { snapshot: Snapshot; progress: Partial<Record<Engine, EngineProgress>>; onAction: (method: string, params?: JsonObject) => Promise<any> }) {
  // A recent check is reused by the service; opening the tab never waits on GitHub twice.
  useEffect(() => { for (const engine of ENGINES) void onAction('runtime.check', { engine, force: false }).catch(() => {}); }, []);
  return <div className="page-content runtime-content">
    <div className="page-title"><div>
      <div className="eyebrow">INFERENCE ENGINES</div>
      <h1>Runtime</h1>
      <p>The engines that run your models. Several can serve at once: each server runs on the engine its profile names, behind the same Studio API.</p>
    </div></div>
    <div className="engine-cards">{ENGINES.map(engine => <EngineCard key={engine} engine={engine} status={snapshot.engines?.[engine]} progress={progress[engine]} snapshot={snapshot} onAction={onAction} />)}</div>
  </div>;
}
