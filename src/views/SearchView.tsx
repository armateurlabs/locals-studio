import { useEffect, useRef, useState } from 'react';
import { ArrowDown, ArrowDownToLine, ArrowUpRight, Box, Check, ChevronRight, Download, Heart, Pause, Play, RefreshCw, Search, ShieldCheck, Sparkles, X } from 'lucide-react';
import type { Download as DownloadRecord, JsonObject, Snapshot } from '../types';
import { bytes, downloadProgress, downloadState, modelMetadata } from '../utils.mjs';
import { Empty, Modal, Spinner, Status } from '../components';
import { errorMessage, openExternal, request } from '../api';

interface Props { snapshot: Snapshot; onAction: (method: string, params?: JsonObject) => Promise<any>; }
export default function SearchView({ snapshot, onAction }: Props) {
  const [query, setQuery] = useState('');
  const [results, setResults] = useState<JsonObject[]>([]);
  const [loading, setLoading] = useState(true);
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
    try { const result = await request('search', { query: value }); if (sequence === searchSequence.current) setResults(Array.isArray(result) ? result : result.models ?? result.results ?? []); }
    catch (failure) { if (sequence === searchSequence.current) setError(errorMessage(failure)); }
    finally { if (sequence === searchSequence.current) setLoading(false); }
  };
  useEffect(() => { const timer = window.setTimeout(() => void search(query), 350); return () => { window.clearTimeout(timer); searchSequence.current += 1; }; }, [query]);
  useEffect(() => () => { infoSequence.current += 1; }, []);
  const inspect = async (model: JsonObject) => { const id = model.id ?? model.modelId; const sequence = ++infoSequence.current; setDetailLoading(id); setError(''); try { const info = await request('model.info', { repo_id: id }); if (sequence === infoSequence.current) { setDetails({ ...model, ...info }); setRevision('main'); } } catch (failure) { if (sequence === infoSequence.current) setError(errorMessage(failure)); } finally { if (sequence === infoSequence.current) setDetailLoading(''); } };
  const download = async (repoId: string, ref = 'main') => { setBusy(repoId); setError(''); try { await onAction('download.add', { repo_id: repoId, revision: ref }); setDetails(null); setDownloadsOpen(true); } catch (failure) { setError(errorMessage(failure)); } finally { setBusy(''); } };
  const activeDownloads = snapshot.downloads.filter(item => !['cancelled'].includes(downloadState(item)));
  const pending = activeDownloads.filter(item => !['completed'].includes(downloadState(item)));
  const totalDownloaded = activeDownloads.reduce((sum, item) => sum + (item.downloaded_bytes ?? 0), 0);
  const job = (repoId: string) => snapshot.downloads.find(item => item.repo_id === repoId && downloadState(item) !== 'cancelled');
  const installed = (repoId: string) => snapshot.models.some(model => model.repo_id === repoId);
  const chips = ['Qwen', 'Llama', 'Mistral', 'Gemma', 'DeepSeek'];
  const detailMetadata = details ? modelMetadata(details) : null;

  return <div className="discover-view"><div className="page-content discover-content"><div className="page-title"><div><div className="eyebrow">A WORLD OF LOCAL MODELS</div><h1>Find your next model</h1><p>Explore MLX models from the Hugging Face community.</p></div><div className="source-badge"><span>🤗</span><div>Hugging Face<small>Connected to the Hub</small></div></div></div>
      <div className="discover-search"><Search size={21} /><input autoFocus aria-label="Search Hugging Face models" placeholder="Search models, authors, or paste a repository ID…" value={query} onChange={event => setQuery(event.target.value)} />{query ? <button className="icon-button" aria-label="Clear search" onClick={() => setQuery('')}><X size={16} /></button> : <kbd>MLX</kbd>}</div><div className="discover-filters"><div><span className="format-pill"><span /> MLX</span><span className="subtle">Made for Apple Silicon</span></div><div className="search-chips">{chips.map(chip => <button className={query === chip ? 'selected' : ''} key={chip} onClick={() => setQuery(query === chip ? '' : chip)}>{chip}</button>)}</div></div>
      <div className="section-toolbar"><h2>{query ? 'Search results' : 'Popular in the community'}{!loading && <span className="count-badge">{results.length}</span>}</h2><span className="subtle">Sorted by downloads <ArrowDown size={12} /></span></div>
      {error && <div className="inline-error" role="alert">{error}<button className="text-button" onClick={() => void search(query)}>Try again</button></div>}
      {loading ? <div className="search-loading"><Spinner size={23} /><span>Looking through the Hub…</span></div> : results.length ? <div className="discover-grid">{results.map(result => {
        const id = result.id ?? result.modelId;
        const [author, ...parts] = String(id).split('/');
        const name = parts.join('/') || author;
        const downloadJob = job(id);
        const tags: string[] = result.tags ?? [];
        const quant = tags.find(tag => /\d[- ]?bit|\d+bit|q[248]/i.test(tag)) ?? name.match(/\d+bit|\d+-bit|bf16|fp16/i)?.[0];
        return <article className="discover-card" key={id}><div className="discover-card-top"><div className="publisher-avatar">{author.slice(0, 1).toUpperCase()}</div><span>{author}</span><span className="tag">MLX</span></div><button className="discover-card-title" onClick={() => void inspect(result)}>{name}</button><p>{result.pipeline_tag?.replaceAll('-', ' ') ?? 'Local language model'} <span className="dot-separator">·</span> Apple Silicon</p><div className="model-tags"><span>safetensors</span>{quant && <span>{quant}</span>}{tags.includes('vision') && <span>vision</span>}</div><div className="discover-card-bottom"><div className="model-popularity"><span title="Downloads"><ArrowDownToLine size={13} />{compact(result.downloads ?? 0)}</span><span title="Likes"><Heart size={12} />{compact(result.likes ?? 0)}</span></div><button disabled={busy === id || detailLoading === id || installed(id) || !!downloadJob && downloadState(downloadJob) !== 'failed'} onClick={() => void inspect(result)}>{busy === id || detailLoading === id ? <Spinner size={13} /> : installed(id) || downloadJob && downloadState(downloadJob) === 'completed' ? <Check size={13} /> : downloadJob ? <Download size={13} /> : <ArrowDownToLine size={13} />}{installed(id) ? 'Installed' : downloadJob ? downloadState(downloadJob) === 'completed' ? 'Downloaded' : 'In downloads' : 'Get model'}</button></div></article>;
      })}</div> : <Empty icon={<Search size={27} />} title="No models found">Try a model family, author, or repository name.</Empty>}
      <div className="hub-note"><ShieldCheck size={16} /><span>Downloads are pinned to a revision and verified before appearing in your library. Model compatibility depends on Locals.</span></div>
    </div>
    {activeDownloads.length > 0 && <section className={`downloads-panel ${!downloadsOpen ? 'collapsed' : ''}`}><div className="downloads-heading"><button onClick={() => setDownloadsOpen(!downloadsOpen)}><ArrowDownToLine size={16} /><strong>Downloads</strong><span className="count-badge">{pending.length}</span><ChevronRight className={downloadsOpen ? 'rotate' : ''} size={14} /></button><span>{bytes(totalDownloaded)} downloaded <span className="dot-separator">·</span> Continues when you close Studio</span></div>{downloadsOpen && <div className="download-list">{activeDownloads.map(item => <DownloadRow key={item.id} download={item} onAction={onAction} />)}</div>}</section>}
    {details && <Modal title="Download model" onClose={() => { infoSequence.current += 1; setDetails(null); }} wide><div className="settings-body"><div className="model-download-heading"><div className="publisher-avatar large">{String(details.id ?? details.modelId ?? '').slice(0, 1).toUpperCase()}</div><div><small>{String(details.id ?? details.modelId).split('/')[0]}</small><h3>{String(details.id ?? details.modelId).split('/').slice(1).join('/')}</h3></div><span className="tag">MLX</span></div><div className="download-details-grid"><div><small>Parameters</small><strong>{details.safetensors?.total ? compact(details.safetensors.total) : 'See model card'}</strong></div><div><small>Model weights</small><strong>{detailMetadata?.weightsBytes ? `${detailMetadata.sizeComplete ? '' : '≥ '}${bytes(detailMetadata.weightsBytes)}` : 'Size unavailable'}</strong></div><div><small>License</small><strong>{details.cardData?.license ?? (details.tags ?? []).find((tag: string) => tag.startsWith('license:'))?.replace('license:', '') ?? 'See model card'}</strong></div></div><div className="model-file-summary"><span>{detailMetadata?.quantization ?? 'Quantization not specified'}</span><span>{detailMetadata?.weights.length ?? 0} weight file{detailMetadata?.weights.length === 1 ? '' : 's'}</span><span>{detailMetadata?.files.length ?? 0} repository files</span></div>{details.sha && <label className="field path-detail">Current commit<code>{details.sha}</code></label>}<label className="field">Revision<input value={revision} onChange={event => setRevision(event.target.value)} placeholder="main" /><small>A branch, tag, or commit. Studio pins the immutable commit before downloading.</small></label><p className="subtle">The complete model bundle includes weights, tokenizer, and configuration. Once downloaded, Studio checks the model and adds it to your library.</p><button className="text-link variant-link" onClick={() => { setQuery(String(details.id ?? details.modelId).split('/').at(-1)!.replace(/[-_]?(?:\d+[-_]?bit|bf16|fp16|mlx)/ig, '')); setDetails(null); }}>Find published variants <Search size={13} /></button><a className="text-link" href={`https://huggingface.co/${details.id ?? details.modelId}`} onClick={event => { event.preventDefault(); void openExternal(`https://huggingface.co/${details.id ?? details.modelId}`).catch(failure => setError(errorMessage(failure))); }} target="_blank" rel="noreferrer">Read the model card <ArrowUpRight size={14} /></a>{error && <div className="inline-error">{error}</div>}</div><footer className="modal-footer"><button onClick={() => setDetails(null)}>Cancel</button><button className="primary" disabled={!!busy || !revision.trim()} onClick={() => void download(details.id ?? details.modelId, revision.trim())}>{busy ? <Spinner size={15} /> : <ArrowDownToLine size={15} />} Download model</button></footer></Modal>}
  </div>;
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
