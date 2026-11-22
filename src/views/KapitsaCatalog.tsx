import { useEffect, useState } from 'react';
import { ArrowDownToLine, Boxes, Check, RefreshCw, Square } from 'lucide-react';
import type { JsonObject, KapitsaPull, Snapshot } from '../types';
import { EngineBadge, Empty, Spinner } from '../components';
import { errorMessage, request } from '../api';
import { bytes } from '../utils.mjs';

/** Kapitsa states sizes in decimal GB, as Hugging Face does. */
const gigabytes = (value: number) => `${(value / 1e9).toFixed(1)} GB`;

interface CatalogVariant { name: string; id: string; size_bytes: number; quantization?: string | null; default: boolean; small_mac: boolean; fit: 'fits' | 'tight' | 'too_big'; downloaded: boolean; path?: string | null; }
interface CatalogModel { name: string; alias: string; description: string; default_variant: string; variants: CatalogVariant[]; }
interface Catalog { version: string; ram_bytes: number; models: CatalogModel[]; store?: string; }

const FIT_LABELS: Record<CatalogVariant['fit'], string> = { fits: 'Fits this Mac', tight: 'Tight fit', too_big: 'Too big for this Mac' };

function CatalogCard({ model, ram, pull, onPull, onCancel }: { model: CatalogModel; ram: number; pull?: KapitsaPull; onPull: (variant: CatalogVariant) => Promise<void>; onCancel: (id: string) => Promise<void> }) {
  // Start on the default precision, or the smaller one Kapitsa picks where the default does not fit.
  const initial = model.variants.find(variant => variant.default && variant.fit !== 'too_big') ?? model.variants.find(variant => variant.small_mac) ?? model.variants[0];
  const [variantName, setVariantName] = useState(initial?.name ?? '');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const variant = model.variants.find(item => item.name === variantName) ?? initial;
  const active = pull?.status === 'downloading' && pull.id === variant?.id;
  const share = active && variant?.size_bytes ? Math.min(100, Math.round(pull.received_bytes / variant.size_bytes * 100)) : null;
  const act = async (run: () => Promise<void>) => { setBusy(true); setError(''); try { await run(); } catch (failure) { setError(errorMessage(failure)); } finally { setBusy(false); } };
  return <article className="discover-card kapitsa-card" role="listitem" aria-label={model.name}>
    <div className="discover-card-top"><span className="kapitsa-mark"><Boxes size={15} /></span><span>Kapitsa catalog</span><EngineBadge engine="kapitsa" /></div>
    <div className="discover-card-title">{model.name} <small className="subtle">({model.alias})</small></div>
    <p className="kapitsa-description">{model.description}</p>
    <label className="field kapitsa-variant">Precision<select aria-label={`Precision of ${model.name}`} value={variant?.name ?? ''} onChange={event => setVariantName(event.target.value)}>{model.variants.map(item => <option key={item.name} value={item.name}>{item.name} · {gigabytes(item.size_bytes)}{item.default ? ' · default' : ''}{item.downloaded ? ' · downloaded' : ''}</option>)}</select>{variant?.quantization && <small>{variant.quantization}</small>}</label>
    {variant && <div className="discover-model-stats"><div className="discover-model-size"><span>Download</span><strong>{gigabytes(variant.size_bytes)}</strong><small>with its draft model and MTP head</small></div><div className="discover-model-size"><span>On this Mac</span><strong className={`kapitsa-fit ${variant.fit}`}>{FIT_LABELS[variant.fit]}</strong><small>{Math.round(variant.size_bytes / Math.max(ram, 1) * 100)}% of {bytes(ram)} memory</small></div></div>}
    {active && <div className="kapitsa-pull-progress" role="status"><div className="metric-track"><span style={{ width: `${share ?? 0}%` }} /></div><small>{gigabytes(pull!.received_bytes)} of {gigabytes(variant!.size_bytes)}{share != null && ` · ${share}%`}</small></div>}
    {pull && pull.id === variant?.id && pull.status === 'error' && <p className="inline-error" role="alert">{pull.error}</p>}
    {pull && pull.id === variant?.id && pull.status === 'completed' && <p className="subtle tiny" role="status"><Check size={12} /> Downloaded and added to My models with a Kapitsa profile.</p>}
    {error && <p className="inline-error" role="alert">{error}</p>}
    <div className="discover-card-bottom">
      {variant?.downloaded ? <span className="tag green"><Check size={12} /> In your store</span>
        : active ? <button disabled={busy} onClick={() => void act(() => onCancel(pull!.id))}>{busy ? <Spinner size={14} /> : <Square size={12} />} Pause</button>
        : <button className="primary" disabled={busy || !variant} title={variant?.fit === 'too_big' ? 'Kapitsa may refuse a model that does not fit this Mac.' : undefined} onClick={() => variant && void act(() => onPull(variant))}>{busy ? <Spinner size={14} /> : <ArrowDownToLine size={14} />} Download</button>}
    </div>
  </article>;
}

/** Kapitsa's own models, with the companion files that speed them up, downloaded into its store. */
export default function KapitsaCatalog({ snapshot, onAction, onRuntime }: { snapshot: Snapshot; onAction: (method: string, params?: JsonObject) => Promise<any>; onRuntime?: () => void }) {
  const installed = snapshot.engines?.kapitsa?.installed === true;
  const version = snapshot.engines?.kapitsa?.version;
  const [catalog, setCatalog] = useState<Catalog | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const load = async () => {
    setLoading(true); setError('');
    try { setCatalog(await request<Catalog>('kapitsa.catalog')); }
    catch (failure) { setError(errorMessage(failure)); }
    finally { setLoading(false); }
  };
  const pulls = snapshot.kapitsa_pulls ?? [];
  const completed = pulls.filter(pull => pull.status === 'completed').length;
  // Reload on a new version and after a download completes, to mark it downloaded.
  useEffect(() => { if (installed) void load(); }, [installed, version, completed]);
  if (!installed) return <Empty icon={<Boxes size={27} />} title="Install Kapitsa to browse its catalog" action={onRuntime && <button className="primary" onClick={onRuntime}>Open Runtime</button>}>Kapitsa's catalog lists Qwen, Nemotron, Flint and other models packed for it, with their draft models. Install Kapitsa in Runtime first.</Empty>;
  return <div className="kapitsa-catalog">
    <div className="section-toolbar discover-toolbar"><h2>Kapitsa catalog{catalog && <span className="count-badge">{catalog.models.length}</span>}{loading && <span className="catalogue-refresh" role="status"><Spinner size={13} /> Loading…</span>}</h2><button className="icon-button" aria-label="Refresh Kapitsa catalog" title="Refresh" disabled={loading} onClick={() => void load()}><RefreshCw size={15} /></button></div>
    <p className="catalogue-sort-note">From Kapitsa {catalog?.version ?? version}. Downloads go to Kapitsa's store{catalog?.store ? ` (${catalog.store})` : ''}, shared with the Kapitsa command line, and appear in My models. The fit is the download against this Mac's memory; Kapitsa checks the exact plan before loading.</p>
    {error && <div className="inline-error" role="alert">{error}<button className="text-button" onClick={() => void load()}>Try again</button></div>}
    {catalog && <div className="discover-grid" role="list" aria-label="Kapitsa catalog">{catalog.models.map(model => <CatalogCard key={model.name} model={model} ram={catalog.ram_bytes}
      pull={pulls.filter(pull => model.variants.some(variant => variant.id === pull.id)).sort((a, b) => (a.status === 'downloading' ? -1 : 0) - (b.status === 'downloading' ? -1 : 0))[0]}
      onPull={async variant => { await onAction('kapitsa.pull', { model: variant.id, size_bytes: variant.size_bytes }); }}
      onCancel={async id => { await onAction('kapitsa.pull_cancel', { id }); }} />)}</div>}
  </div>;
}
