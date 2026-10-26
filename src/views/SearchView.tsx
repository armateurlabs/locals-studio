import { useCallback, useEffect, useRef, useState } from 'react';
import { ArrowDown, ArrowDownToLine, ArrowDownUp, ArrowUp, ArrowUpRight, Box, Check, ChevronRight, Download, HardDrive, Heart, LayoutGrid, List, Pause, Play, RefreshCw, Search, ShieldCheck, X } from 'lucide-react';
import type { Download as DownloadRecord, JsonObject, Snapshot } from '../types';
import { bytes, downloadProgress, downloadState, modelMetadata, modelSizeLabel } from '../utils.mjs';
import { Empty, Modal, Spinner, Status } from '../components';
import { errorMessage, openExternal, request } from '../api';
import ModelLogo from '../ModelLogo';
import { catalogueBits, catalogueCompatibility, filterPublicationDates, publicationDate, readCatalogue, writeCatalogue, readDiscoverPreferences, writeDiscoverPreferences, sortCatalogue } from '../catalogue';
import type { CatalogueDirection, CatalogueSort } from '../catalogue';
import { modelParameters } from '../model-parameters';

const SORT_COLUMNS: { key: CatalogueSort; label: string; description: string }[] = [
  { key: 'downloads', label: 'Downloads', description: 'Hub download count' },
  { key: 'bits', label: 'Bits', description: 'Quantization bits' },
  { key: 'parameters', label: 'Parameters', description: 'Published model size in parameters' },
  { key: 'disk', label: 'Download size', description: 'Weights, tokenizer and configuration; decimal GB' },
  { key: 'published', label: 'Published', description: 'Hugging Face repository publication date (UTC)' },
];

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
  for (const field of ['siblings', 'studio_size', 'studio_compatibility', 'cardData', 'safetensors', 'sha', 'createdAt', 'created_at']) {
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
function acquireInfo(model: JsonObject, selectedRevision?: string) {
  const { id, revision: modelRevision } = metadataIdentity(model);
  const revision = selectedRevision ?? modelRevision;
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
  const [dateFrom, setDateFrom] = useState(initial.dateFrom ?? '');
  const [dateTo, setDateTo] = useState(initial.dateTo ?? '');
  const [results, setResults] = useState<JsonObject[]>(() => readCatalogue(initial.query)?.results ?? []);
  const [loading, setLoading] = useState(() => !readCatalogue(initial.query));
  const [error, setError] = useState('');
  const [busy, setBusy] = useState('');
  const [details, setDetails] = useState<JsonObject | null>(null);
  const [detailLoading, setDetailLoading] = useState('');
  const [revision, setRevision] = useState('main');
  const [validatedRevision, setValidatedRevision] = useState('');
  const [revisionChecking, setRevisionChecking] = useState(false);
  const [revisionError, setRevisionError] = useState('');
  const [metadataChecks, setMetadataChecks] = useState<Record<string, string>>({});
  const [metadataAttempt, setMetadataAttempt] = useState(0);
  const metadataResources = useRef(new Map<string, ReturnType<typeof acquireInfo>>());
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
  useEffect(() => { writeDiscoverPreferences({ query, view, sort, direction, dateFrom, dateTo }); }, [query, view, sort, direction, dateFrom, dateTo]);
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
  const unresolvedMetadata = results.filter(result => catalogueCompatibility(result).status === 'unknown'
    || sort === 'parameters' && modelParameters(result).count == null
    || sort === 'disk' && !modelMetadata(result).downloadComplete
    || (sort === 'published' || dateFrom || dateTo) && publicationDate(result) === null);
  const unresolvedIdentities = unresolvedMetadata.map(result => { const { id, revision } = metadataIdentity(result); return `${id}@${revision}`; }).join('|');
  const enrichLatest = useRef(enrich);
  enrichLatest.current = enrich;
  useEffect(() => {
    // Unknown candidates are checked before entering the catalogue, including
    // off-screen results. Reconcile by identity so each finished request does
    // not release and restart every remaining queued check.
    const desired = new Set(unresolvedMetadata.map(result => { const { id, revision } = metadataIdentity(result); return `${id}@${revision}`; }));
    for (const [key, resource] of metadataResources.current) {
      if (!desired.has(key)) { resource.release(); metadataResources.current.delete(key); }
    }
    for (const result of unresolvedMetadata) {
      const { id, revision } = metadataIdentity(result);
      const key = `${id}@${revision}`;
      if (metadataResources.current.has(key)) continue;
      const resource = acquireInfo(result);
      metadataResources.current.set(key, resource);
      void resource.promise.then(info => {
        if (metadataResources.current.get(key) !== resource) return;
        setMetadataChecks(previous => ({ ...previous, [key]: catalogueCompatibility(info).status === 'unknown' ? catalogueCompatibility(info).reason ?? 'Compatibility could not be verified.' : '' }));
        enrichLatest.current(id, revision, info);
      }, failure => { if (metadataResources.current.get(key) === resource) setMetadataChecks(previous => ({ ...previous, [key]: errorMessage(failure) })); }).finally(resource.release);
    }
  }, [unresolvedIdentities, sort, dateFrom, dateTo, enrich, metadataAttempt]);
  useEffect(() => () => { for (const resource of metadataResources.current.values()) resource.release(); metadataResources.current.clear(); }, []);
  useEffect(() => () => { infoSequence.current += 1; }, []);
  const inspect = async (model: JsonObject) => { const id = model.id ?? model.modelId; const sequence = ++infoSequence.current; const resource = acquireInfo(model); setDetailLoading(id); setError(''); try { const info = await resource.promise; if (sequence === infoSequence.current) { setDetails({ ...model, ...info, studio_compatibility: info.studio_compatibility ?? { status: 'unknown' } }); setRevision(info.sha ?? 'main'); setValidatedRevision(info.sha ?? 'main'); setRevisionError(''); setRevisionChecking(false); } } catch (failure) { if (sequence === infoSequence.current) setError(errorMessage(failure)); } finally { resource.release(); if (sequence === infoSequence.current) setDetailLoading(''); } };
  const detailId = details ? String(details.id ?? details.modelId ?? '') : '';
  useEffect(() => {
    if (!details || !revision.trim() || revision.trim() === validatedRevision) {
      setRevisionChecking(false);
      if (details && revision.trim() === validatedRevision) setRevisionError('');
      return;
    }
    let mounted = true;
    let resource: ReturnType<typeof acquireInfo> | undefined;
    setRevisionChecking(true); setRevisionError('');
    const selectedRevision = revision.trim();
    const timer = window.setTimeout(() => {
      resource = acquireInfo(details, selectedRevision);
      void resource.promise.then(info => {
        if (!mounted) return;
        setDetails(previous => previous && String(previous.id ?? previous.modelId ?? '') === detailId ? { ...previous, ...info, studio_compatibility: info.studio_compatibility ?? { status: 'unknown' } } : previous);
        setValidatedRevision(selectedRevision);
      }, failure => { if (mounted) setRevisionError(errorMessage(failure)); }).finally(() => { resource?.release(); if (mounted) setRevisionChecking(false); });
    }, 300);
    return () => { mounted = false; window.clearTimeout(timer); resource?.release(); };
  }, [detailId, revision, validatedRevision]);
  const download = async (repoId: string, ref = 'main') => { setBusy(repoId); setError(''); try { await onAction('download.add', { repo_id: repoId, revision: ref }); setDetails(null); setDownloadsOpen(true); } catch (failure) { setError(errorMessage(failure)); } finally { setBusy(''); } };
  const activeDownloads = snapshot.downloads.filter(item => !['cancelled'].includes(downloadState(item)));
  const pending = activeDownloads.filter(item => !['completed'].includes(downloadState(item)));
  const totalDownloaded = activeDownloads.reduce((sum, item) => sum + (item.downloaded_bytes ?? 0), 0);
  const job = (repoId: string) => snapshot.downloads.find(item => item.repo_id === repoId && downloadState(item) !== 'cancelled');
  const installed = (repoId: string) => snapshot.models.some(model => model.repo_id === repoId);
  const chips = ['Qwen', 'Llama', 'Mistral', 'Gemma', 'DeepSeek'];
  const detailMetadata = details ? modelMetadata(details) : null;
  const detailParameters = details ? modelParameters(details) : null;
  const detailPrecision = details ? detailMetadata?.quantization ?? (catalogueBits(details) == null ? 'Unknown' : `${catalogueBits(details)}-bit`) : 'Unknown';
  const detailPublication = details ? publicationDate(details) : null;
  const detailCompatibility = details ? catalogueCompatibility(details) : { status: 'unknown', reason: null };
  const canDownloadRevision = !!details && /^[a-f\d]{40}$/i.test(details.sha ?? '') && !!revision.trim() && revision.trim() === validatedRevision && !revisionChecking && !revisionError && detailCompatibility.status === 'compatible';
  const compatibleResults = results.filter(result => catalogueCompatibility(result).status === 'compatible');
  const filteredResults = filterPublicationDates(compatibleResults, dateFrom, dateTo);
  const sortedResults = sortCatalogue(filteredResults, sort, direction);
  const unknownResults = results.filter(result => catalogueCompatibility(result).status === 'unknown');
  const unverifiedResults = unknownResults.filter(result => { const { id, revision } = metadataIdentity(result); return !!metadataChecks[`${id}@${revision}`]; });
  const checkingCount = unknownResults.length - unverifiedResults.length;
  const incompatibleCount = results.filter(result => catalogueCompatibility(result).status === 'incompatible').length;
  const invalidDateRange = !!dateFrom && !!dateTo && dateFrom > dateTo;
  const retryChecks = () => {
    for (const result of unverifiedResults) {
      const { id, revision } = metadataIdentity(result);
      metadataCache.delete(`${id}@${revision}`);
      metadataResources.current.get(`${id}@${revision}`)?.release();
      metadataResources.current.delete(`${id}@${revision}`);
    }
    setMetadataChecks({}); setMetadataAttempt(previous => previous + 1);
  };
  const changeSort = (column: CatalogueSort) => {
    if (column === sort) setDirection(previous => previous === 'desc' ? 'asc' : 'desc');
    else { setSort(column); setDirection('desc'); }
  };

  return <div className="discover-view"><div className="page-content discover-content"><div className="page-title"><div><div className="eyebrow">A WORLD OF LOCAL MODELS</div><h1>Find your next model</h1><p>Explore MLX models from the Hugging Face community.</p></div><div className="source-badge"><span>🤗</span><div>Hugging Face<small>Connected to the Hub</small></div></div></div>
      <div className="discover-search"><Search size={21} /><input autoFocus aria-label="Search Hugging Face models" placeholder="Search models, authors, or paste a repository ID…" value={query} onChange={event => setQuery(event.target.value)} />{query ? <button className="icon-button" aria-label="Clear search" onClick={() => setQuery('')}><X size={16} /></button> : <kbd>MLX</kbd>}</div>
      <div className="discover-filters"><div><span className="format-pill"><span /> MLX</span><span className="subtle">Made for Apple Silicon</span></div><div className="catalogue-controls">
        <div className="search-chips" role="group" aria-label="Model families">{chips.map(chip => <button className={query === chip ? 'selected' : ''} aria-pressed={query === chip} key={chip} onClick={() => setQuery(query === chip ? '' : chip)}>{chip}</button>)}</div>
        <button className="icon-button" aria-label="Refresh catalogue" title="Refresh catalogue" disabled={loading} onClick={() => void search(query)}><RefreshCw size={15} /></button>
        <div className="catalogue-view-toggle" role="group" aria-label="Catalogue view"><button className="icon-button" aria-label="Grid view" title="Grid view" aria-pressed={view === 'grid'} onClick={() => setView('grid')}><LayoutGrid size={16} /></button><button className="icon-button" aria-label="List view" title="List view" aria-pressed={view === 'list'} onClick={() => setView('list')}><List size={18} /></button></div>
      </div></div>
      <div className="catalogue-date-filters" role="group" aria-label="Publication date filters"><span>Published <small>UTC</small></span><label>From<input type="date" aria-label="Published from" value={dateFrom} max={dateTo || undefined} onChange={event => setDateFrom(event.target.value)} /></label><label>Through<input type="date" aria-label="Published through" value={dateTo} min={dateFrom || undefined} aria-invalid={invalidDateRange} onChange={event => setDateTo(event.target.value)} /></label>{(dateFrom || dateTo) && <button className="text-button" onClick={() => { setDateFrom(''); setDateTo(''); }}>Clear dates</button>}<small>Repository publication date; updates do not change it.</small></div>
      {invalidDateRange && <p className="inline-error" role="alert">Choose a publication end date on or after the start date.</p>}
      <div className="section-toolbar discover-toolbar"><h2>{query ? 'Search results' : 'Popular in the community'}{filteredResults.length > 0 && <span className="count-badge">{filteredResults.length}</span>}{loading && results.length > 0 && <span className="catalogue-refresh" role="status"><Spinner size={13} /> Updating…</span>}</h2></div>
      {error && <div className="inline-error" role="alert">{error}<button className="text-button" onClick={() => void search(query)}>Try again</button></div>}
      {(checkingCount > 0 || unverifiedResults.length > 0 || incompatibleCount > 0) && <div className="catalogue-validation-summary"><span>Only verified, complete MLX models are listed.</span>{checkingCount > 0 && <span role="status"><Spinner size={13} /> Checking {checkingCount} {checkingCount === 1 ? 'repository' : 'repositories'}…</span>}{unverifiedResults.length > 0 && <span>{unverifiedResults.length} could not be verified. <button className="text-button" onClick={retryChecks}>Retry checks</button></span>}{incompatibleCount > 0 && <span>{incompatibleCount} incompatible {incompatibleCount === 1 ? 'repository excluded' : 'repositories excluded'}.</span>}</div>}
      {loading && !results.length ? <div className="search-loading"><Spinner size={23} /><span>Looking through the Hub…</span></div> : filteredResults.length ? <>
        <p className="catalogue-sort-note">{sort === 'disk' ? 'Download sizes use decimal GB. Complete sizes appear first; partial and unavailable sizes follow. ' : ''}Sorting {filteredResults.length} verified models from the {results.length} results returned by the Hub.{(dateFrom || dateTo) && ' Models without a known publication date are excluded by this filter.'}</p>
        {view === 'grid' && <div className="catalogue-grid-sort" role="group" aria-label="Sort models">{SORT_COLUMNS.map(column => <CatalogueSortButton key={column.key} column={column} sort={sort} direction={direction} onSort={changeSort} />)}</div>}
        <div className={view === 'list' ? 'discover-list-scroll' : undefined}><div className={view === 'list' ? 'discover-list' : 'discover-grid'} role={view === 'list' ? 'table' : 'list'} aria-label="Model catalogue">
          {view === 'list' && <div className="discover-list-heading" role="row"><span role="columnheader">Model</span>{SORT_COLUMNS.map(column => <div className="catalogue-sort-heading" role="columnheader" key={column.key} aria-sort={sort === column.key ? direction === 'asc' ? 'ascending' : 'descending' : 'none'}><CatalogueSortButton column={column} sort={sort} direction={direction} onSort={changeSort} /></div>)}<span role="columnheader" aria-label="Actions" /></div>}
          {sortedResults.map(result => {
            const { id, revision } = metadataIdentity(result);
            return <SearchCard key={`${id}@${revision}`} result={result} view={view} downloadJob={job(id)} installed={installed(id)} busy={busy === id || detailLoading === id} onInspect={() => void inspect(result)} onEnrich={enrich} />;
          })}
        </div></div></> : checkingCount > 0 ? <div className="search-loading"><Spinner size={23} /><span>Checking model compatibility…</span></div> : <Empty icon={<Search size={27} />} title={dateFrom || dateTo ? 'No models match these dates' : 'No compatible models found'}>{dateFrom || dateTo ? 'Widen the publication date range or clear the dates. Models with an unknown publication date are excluded.' : unverifiedResults.length ? 'Some repositories could not be verified. Retry the checks or try another search.' : 'Try a model family, author, or repository name.'}</Empty>}
      <div className="hub-note"><ShieldCheck size={16} /><span>Complete MLX bundles and supported Locals architectures are checked before download. Downloads are pinned to a verified revision.</span></div>
    </div>
    {activeDownloads.length > 0 && <section className={`downloads-panel ${!downloadsOpen ? 'collapsed' : ''}`}><div className="downloads-heading"><button onClick={() => setDownloadsOpen(!downloadsOpen)}><ArrowDownToLine size={16} /><strong>Downloads</strong><span className="count-badge">{pending.length}</span><ChevronRight className={downloadsOpen ? 'rotate' : ''} size={14} /></button><span>{bytes(totalDownloaded)} downloaded <span className="dot-separator">·</span> Continues when you close Studio</span></div>{downloadsOpen && <div className="download-list">{activeDownloads.map(item => <DownloadRow key={item.id} download={item} onAction={onAction} />)}</div>}</section>}
    {details && <Modal title="Download model" onClose={() => { infoSequence.current += 1; setDetails(null); }} wide><div className="settings-body"><div className="model-download-heading"><ModelLogo repoId={String(details.id ?? details.modelId ?? '')} size={40} className="publisher-avatar large" /><div><small>{String(details.id ?? details.modelId).split('/')[0]}</small><h3>{String(details.id ?? details.modelId).split('/').slice(1).join('/')}</h3></div><span className="tag">MLX</span></div><div className="download-details-grid discover-download-sizes"><div><small>Parameters</small><strong title={detailParameters?.source ?? 'No reliable parameter count available'}>{detailParameters?.label ?? 'Unknown'}</strong><span className="size-explanation">{detailParameters?.count == null ? 'Parameter size unavailable' : 'Published model size'}</span></div><div><small>Download size</small><strong>{modelSizeLabel(detailMetadata?.downloadBytes, detailMetadata?.downloadComplete)}</strong><span className="size-explanation">Weights, tokenizer and configuration{detailMetadata?.downloadBytes != null && !detailMetadata.downloadComplete ? ' · partial size' : ''}</span></div><div><small>Published</small><strong>{detailPublication ? <time dateTime={new Date(detailPublication.timestamp).toISOString()} title="Hugging Face repository publication date (UTC)">{detailPublication.label}</time> : 'Unknown'}</strong><span className="size-explanation">Repository publication · UTC</span></div><div><small>Precision</small><strong>{detailPrecision}</strong></div><div><small>License</small><strong>{details.cardData?.license ?? (details.tags ?? []).find((tag: string) => tag.startsWith('license:'))?.replace('license:', '') ?? 'See model card'}</strong></div></div><p className="discover-size-note">Sizes use decimal GB (1 GB = 1,000,000,000 bytes). A ≥ value is a partial total; some file sizes are unavailable. Sizes and compatibility are checked again when the selected revision changes.</p><div className="model-file-summary"><span>{detailMetadata?.quantization ?? 'Quantization not specified'}</span><span>{detailMetadata?.weights.length ?? 0} weight file{detailMetadata?.weights.length === 1 ? '' : 's'}</span><span>{detailMetadata?.files.length ?? 0} repository files</span></div>{details.sha && <label className="field path-detail">Current commit<code>{details.sha}</code></label>}<label className="field">Revision<input value={revision} onChange={event => setRevision(event.target.value)} placeholder="main" /><small>A branch, tag, or commit. Studio pins the immutable commit before downloading.</small></label><div className={`model-compatibility ${canDownloadRevision ? 'compatible' : 'unverified'}`} role={revisionError ? 'alert' : 'status'} data-compatibility={canDownloadRevision ? 'compatible' : revisionChecking || revision.trim() !== validatedRevision ? 'checking' : detailCompatibility.status}>{canDownloadRevision ? <><ShieldCheck size={15} /> Complete MLX model · compatible with Locals</> : revisionError ? revisionError : revisionChecking || revision.trim() !== validatedRevision && revision.trim() ? <><Spinner size={15} /> Checking the selected revision…</> : (!revision.trim() ? 'Enter a revision to check compatibility.' : detailCompatibility.reason ?? 'Compatibility has not been verified for this revision.')}</div><p className="subtle">Studio checks the architecture, weights, tokenizer, and configuration before downloading this revision.</p><button className="text-link variant-link" onClick={() => { setQuery(String(details.id ?? details.modelId).split('/').at(-1)!.replace(/[-_]?(?:\d+[-_]?bit|bf16|fp16|mlx)/ig, '')); setDetails(null); }}>Find published variants <Search size={13} /></button><a className="text-link" href={`https://huggingface.co/${details.id ?? details.modelId}`} onClick={event => { event.preventDefault(); void openExternal(`https://huggingface.co/${details.id ?? details.modelId}`).catch(failure => setError(errorMessage(failure))); }} target="_blank" rel="noreferrer">Read the model card <ArrowUpRight size={14} /></a>{error && <div className="inline-error">{error}</div>}</div><footer className="modal-footer"><button onClick={() => setDetails(null)}>Cancel</button><button className="primary" disabled={!!busy || !canDownloadRevision} onClick={() => { if (canDownloadRevision) void download(details.id ?? details.modelId, details.sha); }}>{busy ? <Spinner size={15} /> : <ArrowDownToLine size={15} />} Download model</button></footer></Modal>}
  </div>;
}

function CatalogueSortButton({ column, sort, direction, onSort }: {
  column: typeof SORT_COLUMNS[number]; sort: CatalogueSort; direction: CatalogueDirection; onSort: (sort: CatalogueSort) => void;
}) {
  const active = sort === column.key;
  const ordering = column.key === 'published' ? active && direction === 'asc' ? 'Oldest first. Click for newest first.' : active ? 'Newest first. Click for oldest first.' : 'Click for newest first.' : active && direction === 'asc' ? 'Smallest first. Click for largest first.' : active ? 'Largest first. Click for smallest first.' : 'Click for largest first.';
  return <button type="button" className={`catalogue-column-sort ${active ? 'active' : ''}`} data-sort={column.key} aria-label={`Sort by ${column.label}`} aria-pressed={active} title={`${column.description}. ${ordering}`} onClick={() => onSort(column.key)}><span>{column.label}</span>{!active ? <ArrowDownUp size={12} aria-hidden="true" /> : direction === 'asc' ? <ArrowUp size={12} aria-hidden="true" /> : <ArrowDown size={12} aria-hidden="true" />}</button>;
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
  const needsInfo = !modelMetadata(result).downloadComplete || modelParameters(result).count == null || publicationDate(result) === null;
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
  const displayInfo = { ...result, ...info };
  const metadata = modelMetadata(displayInfo);
  const parameters = modelParameters(displayInfo);
  const publication = publicationDate(displayInfo);
  const compatible = catalogueCompatibility(displayInfo).status === 'compatible';
  const checking = needsInfo && !settled;
  const [author, ...parts] = id.split('/');
  const name = parts.join('/') || author;
  const tags: string[] = result.tags ?? [];
  const bits = catalogueBits({ ...result, ...info });
  const quant = metadata.quantization ?? tags.find(tag => /\d[- ]?bit|\d+bit|q[248]/i.test(tag)) ?? name.match(/\d+bit|\d+-bit|bf16|fp16/i)?.[0] ?? (bits == null ? undefined : `${bits}-bit`);
  const downloadButton = <button className="catalogue-get-model" disabled={!compatible || busy || installed || !!downloadJob && downloadState(downloadJob) !== 'failed'} onClick={onInspect}>{busy ? <Spinner size={13} /> : installed || downloadJob && downloadState(downloadJob) === 'completed' ? <Check size={13} /> : downloadJob ? <Download size={13} /> : <ArrowDownToLine size={13} />}{installed ? 'Installed' : downloadJob ? downloadState(downloadJob) === 'completed' ? 'Downloaded' : 'In downloads' : 'Get model'}</button>;
  if (view === 'list') return <article ref={article} className="discover-list-row" role="row" aria-label={id}>
    <div className="catalogue-model" role="cell"><ModelLogo repoId={id} name={name} size={32} /><div><button className="discover-card-title" onClick={onInspect}>{name}</button><small>{author} · MLX</small></div></div>
    <span role="cell" className="catalogue-number" title={`${result.downloads ?? 0} downloads`}><ArrowDownToLine size={13} />{compact(result.downloads ?? 0)}</span>
    <span role="cell" className="catalogue-quantization">{quant ?? 'Unknown'}</span>
    <span role="cell" className="catalogue-number catalogue-parameters" aria-label={`Parameters for ${id}`} title={parameters.source ?? 'No reliable parameter count available'}>{parameters.label}</span>
    <span role="cell" className="catalogue-number" aria-label={`Download size for ${id}`} title={sizeError || 'Weights, tokenizer and configuration; decimal GB'}>{metadata.downloadBytes ? modelSizeLabel(metadata.downloadBytes, metadata.downloadComplete) : checking ? <Spinner size={13} /> : 'Unavailable'}</span>
    <span role="cell" className="catalogue-number catalogue-publication" aria-label={`Publication date for ${id}`} title="Hugging Face repository publication date (UTC)">{publication ? <time dateTime={new Date(publication.timestamp).toISOString()}>{publication.label}</time> : 'Unknown'}</span>
    <div className="catalogue-row-action" role="cell">{downloadButton}</div>
  </article>;
  return <article ref={article} className="discover-card" role="listitem">
    <div className="discover-card-top"><ModelLogo repoId={id} name={name} className="publisher-avatar" size={24} /><span>{author}</span><span className="tag">MLX</span></div>
    <button className="discover-card-title" onClick={onInspect}>{name}</button>
    <p>{result.pipeline_tag?.replaceAll('-', ' ') ?? 'Local language model'} <span className="dot-separator">·</span> Apple Silicon</p>
    <div className="model-tags"><span>safetensors</span>{quant && <span>{quant}</span>}{tags.includes('vision') && <span>vision</span>}</div>
    <div className="discover-model-stats">
      <div className="discover-model-size discover-parameter-size" aria-label={`Parameters for ${id}`}>
        <span>Parameters</span><strong className={parameters.count == null ? 'size-unavailable' : undefined} title={parameters.source ?? 'No reliable parameter count available'}>{parameters.label}</strong>
        <small>{parameters.count == null ? 'Parameter size unavailable' : 'Published model size'}</small>
      </div>
      <div className="discover-model-size" aria-label={`Download size for ${id}`}>
        <span><HardDrive size={14} /> Download size</span>
        <strong className={metadata.downloadBytes == null ? 'size-unavailable' : undefined} title={sizeError || 'Weights, tokenizer and configuration. GB uses 1,000,000,000 bytes.'}>{metadata.downloadBytes != null ? modelSizeLabel(metadata.downloadBytes, metadata.downloadComplete) : checking ? <><Spinner size={13} /> Checking size…</> : 'Size unavailable'}</strong>
        <small>{metadata.downloadBytes != null && !metadata.downloadComplete ? 'Partial size · some files are unknown' : 'Weights, tokenizer and configuration · decimal GB'}</small>
      </div>
    </div>
    <div className="catalogue-card-publication" aria-label={`Publication date for ${id}`}><span>Published</span> {publication ? <time dateTime={new Date(publication.timestamp).toISOString()} title="Hugging Face repository publication date (UTC)">{publication.label}</time> : <span>Unknown</span>}</div>
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
