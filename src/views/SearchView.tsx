import { useCallback, useEffect, useRef, useState } from 'react';
import { ArrowDown, ArrowDownToLine, ArrowUp, ArrowUpRight, Box, Check, ChevronRight, Download, HardDrive, Heart, LayoutGrid, List, Pause, Play, RefreshCw, Search, ShieldCheck, X } from 'lucide-react';
import type { Download as DownloadRecord, JsonObject, Snapshot } from '../types';
import { bytes, downloadProgress, downloadState, modelMetadata, modelSizeLabel } from '../utils.mjs';
import { Empty, Modal, Spinner, Status } from '../components';
import { errorMessage, openExternal, request } from '../api';
import ModelLogo from '../ModelLogo';
import { readCatalogue, writeCatalogue, readDiscoverPreferences, writeDiscoverPreferences, sortCatalogue } from '../catalogue';
import type { CatalogueSort } from '../catalogue';

// Cache immutable revisions across Discover visits. Requests without a SHA expire quickly.
type CachedInfo = { value?: JsonObject; error?: string; expires: number };
type PendingInfo = { promise: Promise<JsonObject>; consumers: number };
const metadataCache = new Map<string, CachedInfo>();
const pendingMetadata = new Map<string, PendingInfo>();
const metadataQueue: Array<() => void> = [];
let activeMetadata = 0;
const INFO_CONCURRENCY = 3;
const MAX_CACHED_REVISIONS = 128;
const CATALOGUE_FRESH_MS = 5 * 60_000;
const pendingSearches = new Map<string, Promise<JsonObject[]>>();

function mergeFileMetadata(result: JsonObject, info: JsonObject) {
  const enriched = { ...result };
  for (const field of ['siblings', 'studio_size', 'cardData', 'safetensors', 'sha']) {
    if (info[field] !== undefined) enriched[field] = info[field];
  }
  return enriched;
}

function fetchCatalogue(query: string) {
  const key = query.trim().toLowerCase();
  let pending = pendingSearches.get(key);
  if (!pending) {
    pending = request('search', { query }).then(value => {
      const results: JsonObject[] = Array.isArray(value) ? value : value.models ?? value.results ?? [];
      const persisted = new Map((readCatalogue(query)?.results ?? []).map(result => {
        const { id, revision } = metadataIdentity(result);
        return [`${id}@${revision}`, result];
      }));
      const enriched = results.map(result => {
        const { id, revision } = metadataIdentity(result);
        const info = metadataCache.get(`${id}@${revision}`);
        // File metadata for an immutable commit survives relaunches. Preserve it
        // when the search API returns the same revision without file sizes.
        const saved = /^[a-f\d]{40}$/i.test(revision) ? persisted.get(`${id}@${revision}`) : undefined;
        const current = info?.value && info.expires > Date.now() ? info.value : undefined;
        if (!saved && !current) return result;
        const metadata = { ...saved, ...current };
        // Popularity and other search fields must come from the fresh response.
        return mergeFileMetadata(result, metadata);
      });
      writeCatalogue(query, enriched);
      return enriched;
    });
    pendingSearches.set(key, pending);
    void pending.then(() => pendingSearches.delete(key), () => pendingSearches.delete(key));
  }
  return pending;
}

function metadataIdentity(model: JsonObject) {
  return { id: String(model.id ?? model.modelId ?? ''), revision: String(model.studio_size?.revision ?? model.sha ?? 'main') };
}
function cacheInfo(key: string, entry: CachedInfo) {
  metadataCache.delete(key);
  metadataCache.set(key, entry);
  while (metadataCache.size > MAX_CACHED_REVISIONS) metadataCache.delete(metadataCache.keys().next().value!);
}
function drainMetadataQueue() {
  while (activeMetadata < INFO_CONCURRENCY && metadataQueue.length) metadataQueue.shift()!();
}
function limitedMetadata(task: () => Promise<JsonObject>): Promise<JsonObject> {
  return new Promise((resolve, reject) => {
    metadataQueue.push(() => {
      activeMetadata += 1;
      void Promise.resolve().then(task).then(resolve, reject).finally(() => {
        activeMetadata -= 1;
        drainMetadataQueue();
      });
    });
    drainMetadataQueue();
  });
}
function acquireInfo(model: JsonObject) {
  const { id, revision } = metadataIdentity(model);
  const key = `${id}@${revision}`;
  const cached = metadataCache.get(key);
  if (cached && cached.expires > Date.now()) {
    return { promise: cached.value ? Promise.resolve(cached.value) : Promise.reject(new Error(cached.error)), release: () => {} };
  }
  metadataCache.delete(key);
  let pending = pendingMetadata.get(key);
  if (!pending) {
    const entry: PendingInfo = { consumers: 0, promise: Promise.resolve({}) };
    entry.promise = limitedMetadata(async () => {
      // A query change can remove queued cards before their request starts.
      if (entry.consumers === 0) throw new Error('Metadata no longer needed');
      try {
        const info = await request<JsonObject>('model.info', { repo_id: id, revision });
        if (/^[a-f\d]{40}$/i.test(revision) && info.sha !== revision) throw new Error('The Hub returned a different revision.');
        cacheInfo(key, { value: info, expires: /^[a-f\d]{40}$/i.test(revision) ? Infinity : Date.now() + 60_000 });
        if (typeof info.sha === 'string' && /^[a-f\d]{40}$/i.test(info.sha)) cacheInfo(`${id}@${info.sha}`, { value: info, expires: Infinity });
        return info;
      } catch (failure) {
        cacheInfo(key, { error: errorMessage(failure), expires: Date.now() + 30_000 });
        throw failure;
      }
    });
    pendingMetadata.set(key, entry);
    void entry.promise.then(() => pendingMetadata.delete(key), () => pendingMetadata.delete(key));
    pending = entry;
  }
  pending.consumers += 1;
  let released = false;
  return { promise: pending.promise, release: () => { if (!released) { pending!.consumers -= 1; released = true; } } };
}

interface Props { snapshot: Snapshot; onAction: (method: string, params?: JsonObject) => Promise<any>; }
export default function SearchView({ snapshot, onAction }: Props) {
  const [initial] = useState(readDiscoverPreferences);
  const [query, setQuery] = useState(initial.query);
  const [view, setView] = useState(initial.view);
  const [sort, setSort] = useState(initial.sort);
  const [direction, setDirection] = useState(initial.direction);
  const [results, setResults] = useState<JsonObject[]>(() => readCatalogue(initial.query)?.results ?? []);
  const [loading, setLoading] = useState(() => !readCatalogue(initial.query));
  const [error, setError] = useState('');
  const [busy, setBusy] = useState('');
  const [details, setDetails] = useState<JsonObject | null>(null);
  const [detailLoading, setDetailLoading] = useState('');
  const [revision, setRevision] = useState('main');
  const [downloadsOpen, setDownloadsOpen] = useState(true);
  const searchSequence = useRef(0);
  const infoSequence = useRef(0);
  const search = async (value: string) => {
    const sequence = ++searchSequence.current;
    setLoading(true); setError('');
    try { const result = await fetchCatalogue(value); if (sequence === searchSequence.current) setResults(result); }
    catch (failure) { if (sequence === searchSequence.current) setError(errorMessage(failure)); }
    finally { if (sequence === searchSequence.current) setLoading(false); }
  };
  useEffect(() => {
    searchSequence.current += 1;
    const cached = readCatalogue(query);
    setResults(cached?.results ?? []);
    setError('');
    if (cached && Date.now() - cached.updatedAt < CATALOGUE_FRESH_MS) { setLoading(false); return () => { searchSequence.current += 1; }; }
    setLoading(true);
    const timer = window.setTimeout(() => void search(query), cached ? 0 : 350);
    return () => { window.clearTimeout(timer); searchSequence.current += 1; };
  }, [query]);
  useEffect(() => { writeDiscoverPreferences({ query, view, sort, direction }); }, [query, view, sort, direction]);
  const enrich = useCallback((id: string, revision: string, info: JsonObject) => {
    setResults(previous => {
      const next = previous.map(result => {
        const identity = metadataIdentity(result);
        return identity.id === id && identity.revision === revision ? mergeFileMetadata(result, info) : result;
      });
      // Keep the search timestamp: metadata requests must not postpone catalogue refreshes.
      const cached = readCatalogue(query);
      if (cached) writeCatalogue(query, next, cached.updatedAt);
      return next;
    });
  }, [query]);
  const unresolvedSizes = results.filter(result => {
    const metadata = modelMetadata(result);
    return !metadata.sizeComplete || !metadata.downloadComplete;
  });
  const unresolvedIdentities = unresolvedSizes.map(result => { const { id, revision } = metadataIdentity(result); return `${id}@${revision}`; }).join('|');
  useEffect(() => {
    if (sort !== 'weights' && sort !== 'disk') return;
    let mounted = true;
    // A size sort needs metadata for the whole displayed result set, including off-screen rows.
    const resources = unresolvedSizes.map(result => {
      const { id, revision } = metadataIdentity(result);
      const resource = acquireInfo(result);
      void resource.promise.then(info => { if (mounted) enrich(id, revision, info); }, () => {}).finally(resource.release);
      return resource;
    });
    return () => { mounted = false; resources.forEach(resource => resource.release()); };
  }, [unresolvedIdentities, sort, enrich]);
  useEffect(() => () => { infoSequence.current += 1; }, []);
  const inspect = async (model: JsonObject) => { const id = model.id ?? model.modelId; const sequence = ++infoSequence.current; const resource = acquireInfo(model); setDetailLoading(id); setError(''); try { const info = await resource.promise; if (sequence === infoSequence.current) { setDetails({ ...model, ...info }); setRevision(info.sha ?? 'main'); } } catch (failure) { if (sequence === infoSequence.current) setError(errorMessage(failure)); } finally { resource.release(); if (sequence === infoSequence.current) setDetailLoading(''); } };
  const download = async (repoId: string, ref = 'main') => { setBusy(repoId); setError(''); try { await onAction('download.add', { repo_id: repoId, revision: ref }); setDetails(null); setDownloadsOpen(true); } catch (failure) { setError(errorMessage(failure)); } finally { setBusy(''); } };
  const activeDownloads = snapshot.downloads.filter(item => !['cancelled'].includes(downloadState(item)));
  const pending = activeDownloads.filter(item => !['completed'].includes(downloadState(item)));
  const totalDownloaded = activeDownloads.reduce((sum, item) => sum + (item.downloaded_bytes ?? 0), 0);
  const job = (repoId: string) => snapshot.downloads.find(item => item.repo_id === repoId && downloadState(item) !== 'cancelled');
  const installed = (repoId: string) => snapshot.models.some(model => model.repo_id === repoId);
  const chips = ['Qwen', 'Llama', 'Mistral', 'Gemma', 'DeepSeek'];
  const detailMetadata = details ? modelMetadata(details) : null;
  const sortedResults = sortCatalogue(results, sort, direction);

  return <div className="discover-view"><div className="page-content discover-content"><div className="page-title"><div><div className="eyebrow">A WORLD OF LOCAL MODELS</div><h1>Find your next model</h1><p>Explore MLX models from the Hugging Face community.</p></div><div className="source-badge"><span>🤗</span><div>Hugging Face<small>Connected to the Hub</small></div></div></div>
      <div className="discover-search"><Search size={21} /><input autoFocus aria-label="Search Hugging Face models" placeholder="Search models, authors, or paste a repository ID…" value={query} onChange={event => setQuery(event.target.value)} />{query ? <button className="icon-button" aria-label="Clear search" onClick={() => setQuery('')}><X size={16} /></button> : <kbd>MLX</kbd>}</div><div className="discover-filters"><div><span className="format-pill"><span /> MLX</span><span className="subtle">Made for Apple Silicon</span></div><div className="search-chips">{chips.map(chip => <button className={query === chip ? 'selected' : ''} key={chip} onClick={() => setQuery(query === chip ? '' : chip)}>{chip}</button>)}</div></div>
      <div className="section-toolbar discover-toolbar"><h2>{query ? 'Search results' : 'Popular in the community'}{results.length > 0 && <span className="count-badge">{results.length}</span>}{loading && results.length > 0 && <span className="catalogue-refresh" role="status"><Spinner size={13} /> Updating…</span>}</h2><div className="catalogue-controls">
        <label className="catalogue-sort">Sort by <select aria-label="Sort models by" value={sort} onChange={event => setSort(event.target.value as CatalogueSort)}><option value="downloads">Downloads</option><option value="weights">Model weights</option><option value="bits">Quantization bits</option><option value="disk">Disk size</option></select></label>
        <button className="icon-button catalogue-direction" aria-label={direction === 'desc' ? 'Sort ascending' : 'Sort descending'} title={direction === 'desc' ? 'Largest first — click for smallest first' : 'Smallest first — click for largest first'} onClick={() => setDirection(direction === 'desc' ? 'asc' : 'desc')}>{direction === 'desc' ? <ArrowDown size={16} /> : <ArrowUp size={16} />}</button>
        <button className="icon-button" aria-label="Refresh catalogue" title="Refresh catalogue" disabled={loading} onClick={() => void search(query)}><RefreshCw size={15} /></button>
        <div className="catalogue-view-toggle" role="group" aria-label="Catalogue view"><button className="icon-button" aria-label="Grid view" title="Grid view" aria-pressed={view === 'grid'} onClick={() => setView('grid')}><LayoutGrid size={16} /></button><button className="icon-button" aria-label="List view" title="List view" aria-pressed={view === 'list'} onClick={() => setView('list')}><List size={18} /></button></div>
      </div></div>
      {error && <div className="inline-error" role="alert">{error}<button className="text-button" onClick={() => void search(query)}>Try again</button></div>}
      {loading && !results.length ? <div className="search-loading"><Spinner size={23} /><span>Looking through the Hub…</span></div> : results.length ? <>
        <p className="catalogue-sort-note">{sort === 'weights' || sort === 'disk' ? 'Sizes use decimal GB. Complete sizes appear first; partial and unavailable sizes follow. ' : ''}Sorting the {results.length} models returned by the Hub.</p>
        <div className={view === 'list' ? 'discover-list-scroll' : undefined}><div className={view === 'list' ? 'discover-list' : 'discover-grid'} aria-label="Model catalogue">
          {view === 'list' && <div className="discover-list-heading" aria-hidden="true"><span>Model</span><span>Downloads</span><span>Bits</span><span title="Safetensors files">Weights</span><span title="Weights, tokenizer and configuration">Disk size</span><span /></div>}
          {sortedResults.map(result => {
            const { id, revision } = metadataIdentity(result);
            return <SearchCard key={`${id}@${revision}`} result={result} view={view} downloadJob={job(id)} installed={installed(id)} busy={busy === id || detailLoading === id} onInspect={() => void inspect(result)} onEnrich={enrich} />;
          })}
        </div></div></> : <Empty icon={<Search size={27} />} title="No models found">Try a model family, author, or repository name.</Empty>}
      <div className="hub-note"><ShieldCheck size={16} /><span>Downloads are pinned to a revision and verified before appearing in your library. Model compatibility depends on Locals.</span></div>
    </div>
    {activeDownloads.length > 0 && <section className={`downloads-panel ${!downloadsOpen ? 'collapsed' : ''}`}><div className="downloads-heading"><button onClick={() => setDownloadsOpen(!downloadsOpen)}><ArrowDownToLine size={16} /><strong>Downloads</strong><span className="count-badge">{pending.length}</span><ChevronRight className={downloadsOpen ? 'rotate' : ''} size={14} /></button><span>{bytes(totalDownloaded)} downloaded <span className="dot-separator">·</span> Continues when you close Studio</span></div>{downloadsOpen && <div className="download-list">{activeDownloads.map(item => <DownloadRow key={item.id} download={item} onAction={onAction} />)}</div>}</section>}
    {details && <Modal title="Download model" onClose={() => { infoSequence.current += 1; setDetails(null); }} wide><div className="settings-body"><div className="model-download-heading"><ModelLogo repoId={String(details.id ?? details.modelId ?? '')} size={40} className="publisher-avatar large" /><div><small>{String(details.id ?? details.modelId).split('/')[0]}</small><h3>{String(details.id ?? details.modelId).split('/').slice(1).join('/')}</h3></div><span className="tag">MLX</span></div><div className="download-details-grid discover-download-sizes"><div><small>Model weights</small><strong>{modelSizeLabel(detailMetadata?.weightsBytes, detailMetadata?.sizeComplete)}</strong><span className="size-explanation">Safetensors files{detailMetadata?.weightsBytes != null && !detailMetadata.sizeComplete ? ' · partial size' : ''}</span></div><div><small>Total download</small><strong>{modelSizeLabel(detailMetadata?.downloadBytes, detailMetadata?.downloadComplete)}</strong><span className="size-explanation">Weights, tokenizer and configuration{detailMetadata?.downloadBytes != null && !detailMetadata.downloadComplete ? ' · partial size' : ''}</span></div><div><small>Parameters</small><strong>{details.safetensors?.total ? compact(details.safetensors.total) : 'See model card'}</strong></div><div><small>License</small><strong>{details.cardData?.license ?? (details.tags ?? []).find((tag: string) => tag.startsWith('license:'))?.replace('license:', '') ?? 'See model card'}</strong></div></div><p className="discover-size-note">Sizes use decimal GB (1 GB = 1,000,000,000 bytes). A ≥ value is a partial total; some file sizes are unavailable. These sizes describe the displayed commit. Changing the revision may change the download size.</p><div className="model-file-summary"><span>{detailMetadata?.quantization ?? 'Quantization not specified'}</span><span>{detailMetadata?.weights.length ?? 0} weight file{detailMetadata?.weights.length === 1 ? '' : 's'}</span><span>{detailMetadata?.files.length ?? 0} repository files</span></div>{details.sha && <label className="field path-detail">Current commit<code>{details.sha}</code></label>}<label className="field">Revision<input value={revision} onChange={event => setRevision(event.target.value)} placeholder="main" /><small>A branch, tag, or commit. Studio pins the immutable commit before downloading.</small></label><p className="subtle">The complete model bundle includes weights, tokenizer, and configuration. Once downloaded, Studio checks the model and adds it to your library.</p><button className="text-link variant-link" onClick={() => { setQuery(String(details.id ?? details.modelId).split('/').at(-1)!.replace(/[-_]?(?:\d+[-_]?bit|bf16|fp16|mlx)/ig, '')); setDetails(null); }}>Find published variants <Search size={13} /></button><a className="text-link" href={`https://huggingface.co/${details.id ?? details.modelId}`} onClick={event => { event.preventDefault(); void openExternal(`https://huggingface.co/${details.id ?? details.modelId}`).catch(failure => setError(errorMessage(failure))); }} target="_blank" rel="noreferrer">Read the model card <ArrowUpRight size={14} /></a>{error && <div className="inline-error">{error}</div>}</div><footer className="modal-footer"><button onClick={() => setDetails(null)}>Cancel</button><button className="primary" disabled={!!busy || !revision.trim()} onClick={() => void download(details.id ?? details.modelId, revision.trim())}>{busy ? <Spinner size={15} /> : <ArrowDownToLine size={15} />} Download model</button></footer></Modal>}
  </div>;
}

function SearchCard({ result, view, downloadJob, installed, busy, onInspect, onEnrich }: {
  result: JsonObject; view: 'grid' | 'list'; downloadJob?: DownloadRecord; installed: boolean; busy: boolean; onInspect: () => void;
  onEnrich: (id: string, revision: string, info: JsonObject) => void;
}) {
  const article = useRef<HTMLElement>(null);
  const [visible, setVisible] = useState(false);
  const [info, setInfo] = useState<JsonObject | null>(null);
  const [settled, setSettled] = useState(false);
  const [sizeError, setSizeError] = useState('');
  const { id, revision } = metadataIdentity(result);
  const needsInfo = !modelMetadata(result).sizeComplete || !modelMetadata(result).downloadComplete;
  useEffect(() => {
    if (!needsInfo) return;
    if (!article.current || typeof IntersectionObserver === 'undefined') { setVisible(true); return; }
    const observer = new IntersectionObserver(entries => {
      if (entries.some(entry => entry.isIntersecting)) { setVisible(true); observer.disconnect(); }
    }, { rootMargin: '250px 0px' });
    observer.observe(article.current);
    return () => observer.disconnect();
  }, [needsInfo]);
  useEffect(() => {
    if (!visible || !needsInfo) return;
    let mounted = true;
    const resource = acquireInfo(result);
    void resource.promise.then(value => { if (mounted) { setInfo(value); onEnrich(id, revision, value); } }, failure => {
      if (mounted) setSizeError(errorMessage(failure));
    }).finally(() => { resource.release(); if (mounted) setSettled(true); });
    return () => { mounted = false; resource.release(); };
  }, [id, revision, visible, needsInfo, onEnrich]);
  const metadata = modelMetadata(info ?? result);
  const checking = needsInfo && !settled;
  const [author, ...parts] = id.split('/');
  const name = parts.join('/') || author;
  const tags: string[] = result.tags ?? [];
  const quant = metadata.quantization ?? tags.find(tag => /\d[- ]?bit|\d+bit|q[248]/i.test(tag)) ?? name.match(/\d+bit|\d+-bit|bf16|fp16/i)?.[0];
  const downloadButton = <button className="catalogue-get-model" disabled={busy || installed || !!downloadJob && downloadState(downloadJob) !== 'failed'} onClick={onInspect}>{busy ? <Spinner size={13} /> : installed || downloadJob && downloadState(downloadJob) === 'completed' ? <Check size={13} /> : downloadJob ? <Download size={13} /> : <ArrowDownToLine size={13} />}{installed ? 'Installed' : downloadJob ? downloadState(downloadJob) === 'completed' ? 'Downloaded' : 'In downloads' : 'Get model'}</button>;
  if (view === 'list') return <article ref={article} className="discover-list-row" aria-label={id}>
    <div className="catalogue-model"><ModelLogo repoId={id} name={name} size={32} /><div><button className="discover-card-title" onClick={onInspect}>{name}</button><small>{author} · MLX</small></div></div>
    <span className="catalogue-number" title={`${result.downloads ?? 0} downloads`}><ArrowDownToLine size={13} />{compact(result.downloads ?? 0)}</span>
    <span className="catalogue-quantization">{quant ?? 'Unknown'}</span>
    <span className="catalogue-number" aria-label={`Model size for ${id}`} title={sizeError || 'Safetensors files; decimal GB'}>{metadata.weightsBytes ? modelSizeLabel(metadata.weightsBytes, metadata.sizeComplete) : checking ? <Spinner size={13} /> : 'Unavailable'}</span>
    <span className="catalogue-number" title={sizeError || 'Weights, tokenizer and configuration; decimal GB'}>{metadata.downloadBytes ? modelSizeLabel(metadata.downloadBytes, metadata.downloadComplete) : checking ? <Spinner size={13} /> : 'Unavailable'}</span>
    {downloadButton}
  </article>;
  return <article ref={article} className="discover-card">
    <div className="discover-card-top"><ModelLogo repoId={id} name={name} className="publisher-avatar" size={24} /><span>{author}</span><span className="tag">MLX</span></div>
    <button className="discover-card-title" onClick={onInspect}>{name}</button>
    <p>{result.pipeline_tag?.replaceAll('-', ' ') ?? 'Local language model'} <span className="dot-separator">·</span> Apple Silicon</p>
    <div className="model-tags"><span>safetensors</span>{quant && <span>{quant}</span>}{tags.includes('vision') && <span>vision</span>}</div>
    <div className="discover-model-size" aria-label={`Model size for ${id}`}>
      <span><HardDrive size={14} /> Model weights</span>
      <strong className={!metadata.weightsBytes ? 'size-unavailable' : undefined} title={sizeError || 'Sum of safetensors file sizes. GB uses 1,000,000,000 bytes.'}>
        {metadata.weightsBytes ? modelSizeLabel(metadata.weightsBytes, metadata.sizeComplete) : checking ? <><Spinner size={13} /> Checking size…</> : 'Size unavailable'}
      </strong>
      <small>{metadata.weightsBytes && !metadata.sizeComplete ? checking ? 'Partial size · checking remaining files…' : 'Partial size · some files are unknown' : metadata.weightsBytes ? 'Decimal GB · weights on disk' : checking ? 'Loading file metadata only' : 'The Hub did not provide weight file sizes'}</small>
      {metadata.downloadBytes != null && <span className="discover-download-total">Total download <b>{modelSizeLabel(metadata.downloadBytes, metadata.downloadComplete)}</b>{!metadata.downloadComplete && <em>partial</em>}</span>}
    </div>
    <div className="discover-card-bottom"><div className="model-popularity"><span title="Downloads"><ArrowDownToLine size={13} />{compact(result.downloads ?? 0)}</span><span title="Likes"><Heart size={12} />{compact(result.likes ?? 0)}</span></div>{downloadButton}</div>
  </article>;
}

function DownloadRow({ download, onAction }: { download: DownloadRecord; onAction: Props['onAction'] }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const state = downloadState(download);
  const progress = downloadProgress(download);
  const perform = async (method: string) => { setBusy(true); setError(''); try { await onAction(method, { id: download.id }); } catch (failure) { setError(errorMessage(failure)); } finally { setBusy(false); } };
  return <div className="download-row"><span className={`download-file-icon ${state === 'completed' ? 'done' : ''}`}>{state === 'completed' ? <Check size={18} /> : <Box size={20} />}</span><div className="download-row-main"><div><strong>{download.repo_id.split('/').at(-1)}</strong><span>{download.repo_id.split('/')[0]}</span><Status state={state} /></div><div className="download-track"><span style={{ width: `${progress}%` }} /></div><small>{bytes(download.downloaded_bytes)} / {bytes(download.total_bytes)} <span> {(download.bytes_per_second ?? 0) > 0 ? `${bytes(download.bytes_per_second)}/s` : ''} {download.eta_seconds ? ` · ${Math.ceil(download.eta_seconds / 60)} min remaining` : ''}</span></small>{state === 'running' && download.transport === 'http_replay' && !((download.bytes_per_second ?? 0) > 0) && <small>Verifying saved data before continuing…</small>}{(download.error || error) && <p className="inline-error">{error || download.error}</p>}</div><span className="download-percent">{progress.toFixed(0)}%</span>{state !== 'completed' && <div className="download-controls">{busy ? <Spinner size={15} /> : <><button className="icon-button" title={['paused', 'failed', 'error'].includes(state) ? 'Resume download' : 'Pause download'} aria-label={`${['paused', 'failed', 'error'].includes(state) ? 'Resume' : 'Pause'} ${download.repo_id}`} onClick={() => void perform(['paused', 'failed', 'error'].includes(state) ? 'download.resume' : 'download.pause')}>{['paused', 'failed', 'error'].includes(state) ? <Play size={15} /> : <Pause size={15} />}</button><button className="icon-button" title="Cancel download" aria-label={`Cancel ${download.repo_id}`} onClick={() => void perform('download.cancel')}><X size={16} /></button></>}</div>}</div>;
}
function compact(value: number) { return Intl.NumberFormat('en', { notation: 'compact', maximumFractionDigits: 1 }).format(value); }
