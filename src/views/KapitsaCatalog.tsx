import { useEffect, useState } from 'react';
import { ArrowDownToLine, Check, Square } from 'lucide-react';
import type { JsonObject, KapitsaPull, Snapshot } from '../types';
import { Empty, Spinner } from '../components';
import { errorMessage, request } from '../api';
import type { CatalogueDirection, CatalogueSort } from '../catalogue';
import KapitsaLogo from '../KapitsaLogo';
import { CatalogueItem, FitBadge } from './CatalogueItem';

/** Kapitsa states sizes in decimal GB, as Hugging Face does. */
const gigabytes = (value: number) => `${(value / 1e9).toFixed(1)} GB`;

export interface CatalogVariant { name: string; id: string; size_bytes: number; quantization?: string | null; default: boolean; small_mac: boolean; fit: 'fits' | 'tight' | 'too_big'; downloaded: boolean; path?: string | null; }
export interface CatalogModel { name: string; alias: string; description: string; default_variant: string; variants: CatalogVariant[]; }
export interface Catalog { version: string; ram_bytes: number; models: CatalogModel[]; store?: string; }

/** Loads Kapitsa's catalog once Kapitsa is installed, and again after each new version or finished download. */
export function useKapitsaCatalog(snapshot: Snapshot) {
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
  const completed = (snapshot.kapitsa_pulls ?? []).filter(pull => pull.status === 'completed').length;
  useEffect(() => { if (installed) void load(); }, [installed, version, completed]);
  return { installed, version, catalog, loading, error, load };
}

/** The variant shown first: the default precision, or the smaller one Kapitsa picks where the default does not fit. */
export function initialVariant(model: CatalogModel) {
  return model.variants.find(variant => variant.default && variant.fit !== 'too_big') ?? model.variants.find(variant => variant.small_mac) ?? model.variants[0];
}

/** Parameters read from Kapitsa's model names: `35b-a3b` is 35B with 3B active. */
export function kapitsaParameters(model: CatalogModel): { label: string; count: number | null } {
  const match = `${model.name}-`.match(/-(\d+(?:\.\d+)?)b(?:-a(\d+(?:\.\d+)?)b)?-/i);
  if (!match) return { label: 'Unknown', count: null };
  return { label: `${match[1]}B${match[2] ? `-A${match[2]}B` : ''}`, count: Number(match[1]) * 1e9 };
}
export function kapitsaBits(variant: CatalogVariant): number | null {
  const name = variant.name.toLowerCase();
  const bits = name.match(/^(?:q|nvfp|mxfp|fp|int)?(\d+)(?:bit)?$/) ?? (variant.quantization ?? '').match(/(\d+)-bit/i);
  if (name === 'bf16') return 16;
  return bits && !name.endsWith('gb') ? Number(bits[1]) : null;
}

export function filterKapitsa(models: CatalogModel[], query: string) {
  const words = query.trim().toLowerCase().split(/\s+/).filter(Boolean);
  return models.filter(model => {
    const text = [model.name, model.alias, model.description, ...model.variants.map(variant => variant.name)].join(' ').toLowerCase();
    return words.every(word => text.includes(word));
  });
}
/** Kapitsa publishes neither popularity nor dates: those orders keep the catalog's own order. */
export function sortKapitsa(models: CatalogModel[], sort: CatalogueSort, direction: CatalogueDirection) {
  const value = (model: CatalogModel) => {
    const variant = initialVariant(model);
    if (sort === 'parameters') return kapitsaParameters(model).count;
    if (sort === 'bits') return variant ? kapitsaBits(variant) : null;
    if (sort === 'disk') return variant?.size_bytes ?? null;
    return null;
  };
  if (sort === 'downloads' || sort === 'published') return models;
  return models.map((model, index) => ({ model, index, value: value(model) })).sort((a, b) => {
    if (a.value == null || b.value == null) return a.value == null && b.value == null ? a.index - b.index : a.value == null ? 1 : -1;
    return (direction === 'asc' ? a.value - b.value : b.value - a.value) || a.index - b.index;
  }).map(entry => entry.model);
}

export function KapitsaUnavailable({ onRuntime }: { onRuntime?: () => void }) {
  return <Empty icon={<KapitsaLogo size={34} />} title="Install Kapitsa to browse its catalog" action={onRuntime && <button className="primary" onClick={onRuntime}>Open Runtime</button>}>Kapitsa's catalog lists Qwen, Nemotron, Flint and other models packed for it, with their draft models. Install Kapitsa in Runtime first.</Empty>;
}

/** A Kapitsa catalog model, shown like any other catalogue entry. */
export function KapitsaItem({ model, view, ram, pulls, onAction }: { model: CatalogModel; view: 'grid' | 'list'; ram: number; pulls: KapitsaPull[]; onAction: (method: string, params?: JsonObject) => Promise<any> }) {
  const initial = initialVariant(model);
  const [variantName, setVariantName] = useState(initial?.name ?? '');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const variant = model.variants.find(item => item.name === variantName) ?? initial;
  const pull = pulls.filter(item => item.id === variant?.id).sort((a, b) => (a.status === 'downloading' ? -1 : 0) - (b.status === 'downloading' ? -1 : 0))[0];
  const active = pull?.status === 'downloading';
  const share = active && variant?.size_bytes ? Math.min(100, Math.round(pull.received_bytes / variant.size_bytes * 100)) : null;
  const act = async (run: () => Promise<void>) => { setBusy(true); setError(''); try { await run(); } catch (failure) { setError(errorMessage(failure)); } finally { setBusy(false); } };
  const parameters = kapitsaParameters(model);
  const bits = variant ? kapitsaBits(variant) : null;
  const action = variant?.downloaded ? <button className="catalogue-get-model" disabled><Check size={13} />Installed</button>
    : active ? <button className="catalogue-get-model" disabled={busy} onClick={() => void act(async () => { await onAction('kapitsa.pull_cancel', { id: pull!.id }); })}>{busy ? <Spinner size={13} /> : <Square size={11} />}Pause</button>
    : <button className="catalogue-get-model" disabled={busy || !variant} title={variant?.fit === 'too_big' ? 'Kapitsa may refuse a model that does not fit this Mac.' : 'Download with its draft model and MTP head'} onClick={() => variant && void act(async () => { await onAction('kapitsa.pull', { model: variant.id, size_bytes: variant.size_bytes }); })}>{busy ? <Spinner size={13} /> : <ArrowDownToLine size={13} />}Get model</button>;
  const status = <>
    {active && <div className="kapitsa-pull-progress" role="status"><div className="metric-track"><span style={{ width: `${share ?? 0}%` }} /></div><small>{gigabytes(pull.received_bytes)} of {gigabytes(variant!.size_bytes)}{share != null && ` · ${share}%`}</small></div>}
    {pull?.status === 'error' && <p className="inline-error" role="alert">{pull.error}</p>}
    {pull?.status === 'completed' && <p className="subtle tiny" role="status"><Check size={12} /> Downloaded and added to My models with a Kapitsa profile.</p>}
    {error && <p className="inline-error" role="alert">{error}</p>}
  </>;
  return <CatalogueItem view={view} id={model.name} name={<>{model.name} <small className="subtle">({model.alias})</small></>} publisher="Kapitsa catalog"
    logo={size => <span className="kapitsa-mark" style={{ width: size, height: size }}><KapitsaLogo size={size} /></span>}
    engines={['kapitsa']} description={model.description}
    tags={['kapitsa', ...(variant ? [variant.name] : [])]}
    variants={model.variants.map(item => ({ value: item.name, label: [item.name, gigabytes(item.size_bytes), item.default ? 'default' : null, item.downloaded ? 'installed' : null].filter(Boolean).join(' · ') }))}
    variant={variant?.name ?? ''} onVariant={setVariantName}
    quantization={variant ? variant.name : 'Unknown'}
    parameters={{ label: parameters.label, title: parameters.count == null ? 'The catalog name does not state a parameter count' : 'From the catalog model name', known: parameters.count != null, note: 'From the catalog name' }}
    size={{ value: variant ? gigabytes(variant.size_bytes) : 'Unavailable', known: !!variant, title: variant?.quantization ?? 'Weights with the draft model and MTP head; decimal GB', note: 'With its draft model and MTP head · decimal GB' }}
    published={<span title="Kapitsa's catalog does not publish dates">—</span>}
    downloads={null} likes={null}
    fit={variant ? <FitBadge fit={variant.fit} size={variant.size_bytes} ram={ram} /> : null}
    action={action} status={status} />;
}
