import { forwardRef, type ReactNode } from 'react';
import { ArrowDownToLine, HardDrive, Heart } from 'lucide-react';
import type { Engine } from '../types';
import { EngineBadge } from '../components';
import { engineLabel } from '../engines';
import type { MemoryFit } from '../catalogue';
import { bytes } from '../utils.mjs';

const FIT_LABELS: Record<MemoryFit, string> = { fits: 'Fits', tight: 'Tight fit', too_big: 'Too big' };

/** A compact verdict on whether a download fits this Mac's memory. */
export function FitBadge({ fit, size, ram, engine }: { fit: MemoryFit | null; size?: number | null; ram?: number | null; engine?: Engine }) {
  if (!fit) return null;
  const share = size && ram ? `${Math.round(size / ram * 100)}% of ${bytes(ram)} memory` : '';
  const verdict = fit === 'fits' ? 'Fits this Mac' : fit === 'tight' ? 'A tight fit for this Mac' : 'Too big for this Mac';
  const explanation = engine === 'locals'
    ? `Locals: weights ${bytes(size ?? undefined)}, plus 2 GiB of runtime headroom, against a GPU limit of ${bytes(ram ?? undefined)}. This is a single-model estimate; concurrent servers and context caches also need memory.`
    : `${verdict}${share ? `: the download is ${share}` : ''}. The engine checks the exact plan before loading.`;
  return <span className={`fit-badge ${fit}`} title={explanation}>{engine ? `${engineLabel(engine)} · ` : ''}{FIT_LABELS[fit]}</span>;
}

export interface CatalogueVariantOption { value: string; label: string; }
/** One catalogue entry, whatever its source: only the content differs. */
export interface CatalogueItemProps {
  id: string;
  name: ReactNode;
  publisher: string;
  logo: (size: number) => ReactNode;
  engines: Engine[];
  description: string;
  tags: string[];
  variants: CatalogueVariantOption[];
  variant: string;
  onVariant: (value: string) => void;
  quantization: string;
  parameters: { label: string; title: string; known: boolean; note?: string };
  size: { value: ReactNode; known: boolean; title: string; note: string };
  published: ReactNode;
  downloads: number | null;
  likes: number | null;
  fit: ReactNode;
  action: ReactNode;
  status?: ReactNode;
  onOpen?: () => void;
}

const compact = (value: number) => Intl.NumberFormat('en', { notation: 'compact', maximumFractionDigits: 1 }).format(value);

function VariantSelect({ id, variants, variant, onVariant }: Pick<CatalogueItemProps, 'id' | 'variants' | 'variant' | 'onVariant'>) {
  if (!variants.length) return null;
  return <select className="catalogue-variant" aria-label={`Variant of ${id}`} disabled={variants.length < 2} title={variants.length < 2 ? 'The only published variant' : `${variants.length} variants`} value={variant} onChange={event => onVariant(event.target.value)}>{variants.map(option => <option key={option.value} value={option.value}>{option.label}</option>)}</select>;
}

export const CatalogueItem = forwardRef<HTMLElement, CatalogueItemProps & { view: 'grid' | 'list' }>(function CatalogueItem(props, ref) {
  const { id, view, onOpen } = props;
  const title = onOpen ? <button className="discover-card-title" onClick={onOpen}>{props.name}</button> : <div className="discover-card-title">{props.name}</div>;
  const downloads = <span title={props.downloads == null ? 'The source does not publish download counts' : `${props.downloads} downloads`}><ArrowDownToLine size={13} />{props.downloads == null ? '—' : compact(props.downloads)}</span>;
  if (view === 'list') return <article ref={ref} className="discover-list-row" role="row" aria-label={id}>
    <div className="catalogue-model" role="cell">{props.logo(32)}<div>{title}<small>{props.publisher} · {props.engines.map(engine => engineLabel(engine)).join(', ') || 'MLX'} {props.fit}</small><VariantSelect {...props} />{props.status}</div></div>
    <span role="cell" className="catalogue-number">{downloads}</span>
    <span role="cell" className="catalogue-quantization">{props.quantization}</span>
    <span role="cell" className="catalogue-number catalogue-parameters" aria-label={`Parameters for ${id}`} title={props.parameters.title}>{props.parameters.label}</span>
    <span role="cell" className="catalogue-number" aria-label={`Download size for ${id}`} title={props.size.title}>{props.size.value}</span>
    <span role="cell" className="catalogue-number catalogue-publication" aria-label={`Publication date for ${id}`}>{props.published}</span>
    <div className="catalogue-row-action" role="cell">{props.action}</div>
  </article>;
  return <article ref={ref} className="discover-card" role="listitem" aria-label={id}>
    <div className="discover-card-top">{props.logo(24)}<span>{props.publisher}</span><span className="engine-badges">{props.engines.map(engine => <EngineBadge key={engine} engine={engine} />)}</span></div>
    {title}
    <p>{props.description}</p>
    <div className="model-tags">{props.tags.map(tag => <span key={tag}>{tag}</span>)}{props.fit}</div>
    <VariantSelect {...props} />
    <div className="discover-model-stats">
      <div className="discover-model-size discover-parameter-size" aria-label={`Parameters for ${id}`}>
        <span>Parameters</span><strong className={props.parameters.known ? undefined : 'size-unavailable'} title={props.parameters.title}>{props.parameters.label}</strong>
        <small>{props.parameters.known ? props.parameters.note ?? 'Published model size' : 'Parameter size unavailable'}</small>
      </div>
      <div className="discover-model-size" aria-label={`Download size for ${id}`}>
        <span><HardDrive size={14} /> Download size</span>
        <strong className={props.size.known ? undefined : 'size-unavailable'} title={props.size.title}>{props.size.value}</strong>
        <small>{props.size.note}</small>
      </div>
    </div>
    <div className="catalogue-card-publication" aria-label={`Publication date for ${id}`}><span>Published</span> {props.published}</div>
    {props.status}
    <div className="discover-card-bottom"><div className="model-popularity">{downloads}<span title={props.likes == null ? 'The source does not publish likes' : 'Likes'}><Heart size={12} />{props.likes == null ? '—' : compact(props.likes)}</span></div>{props.action}</div>
  </article>;
});
