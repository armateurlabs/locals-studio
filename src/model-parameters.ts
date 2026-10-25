import type { JsonObject } from './types';

export interface ModelParameters {
  count: number | null;
  label: string;
  source: string | null;
  estimated: boolean;
}

interface NamedSize { count: number; label: string; effective: boolean; }
const unknown = (): ModelParameters => ({ count: null, label: 'Unknown', source: null, estimated: false });
const record = (value: unknown): value is JsonObject => value !== null && typeof value === 'object' && !Array.isArray(value);

function modelName(value: unknown): string | null {
  if (typeof value !== 'string' || value.length > 2_048) return null;
  // Parameter-like text in a publisher's username is not a model-size declaration.
  const name = value.trim().split('/').at(-1)?.trim();
  return name || null;
}

function namedSizes(name: string): NamedSize[] {
  const sizes = new Map<string, NamedSize>();
  const tokens = /(^|[^a-z\d.])(e?)(\d+(?:\.\d+)?)([bm])(?=$|[^a-z\d.])/gi;
  for (const match of name.matchAll(tokens)) {
    const start = match.index! + match[1].length;
    const before = name.slice(0, start);
    const after = name.slice(match.index! + match[0].length);
    // A3B is an active-expert size, never a total. Also reject explicit negative
    // values and textual byte sizes rather than turning either into parameters.
    if (/(?:^|[-_.\s])(?:a|active)[-_.\s]*$/i.test(before)
      || (name[start - 1] === '-' && (start === 1 || /[-\s(\[]/.test(name[start - 2])))
      || /^[-_\s]*bytes?(?:$|[-_.\s])/i.test(after)
      || /^[-_\s]*active$/i.test(after)) continue;
    const amount = Number(match[3]);
    const unit = match[4].toUpperCase();
    const effective = match[2].toLowerCase() === 'e';
    // E2B/E4B are published effective-size classes, not total parameter counts.
    if (effective && (unit !== 'B' || ![2, 4].includes(amount))) continue;
    const count = amount * (unit === 'B' ? 1e9 : 1e6);
    if (!Number.isSafeInteger(count) || count <= 0) continue;
    const label = `${amount}${unit}${effective ? ' effective' : ''}`;
    sizes.set(`${count}:${effective}`, { count, label, effective });
  }
  return [...sizes.values()];
}

function describe(size: NamedSize, provenance: string): ModelParameters {
  return {
    count: size.count,
    label: size.label,
    source: size.effective
      ? `Published effective size from ${provenance}; this is not the model's total parameter count.`
      : `Published nominal parameter size from ${provenance}; this is not an exact tensor count.`,
    estimated: true,
  };
}

function baseModels(value: unknown): string[] {
  if (typeof value === 'string') return [value];
  if (Array.isArray(value) && value.length <= 32 && value.every(item => typeof item === 'string')) return value;
  return [];
}

/**
 * Prefer published model-size names, including the Hub's documented base_model IDs.
 * MLX quantization packs multiple weights into stored tensor elements, so neither
 * safetensors.total nor download/weight bytes establish the original parameter count.
 * No numeric metadata fallback is used without a verified original-parameter meaning.
 */
export function modelParameters(model: JsonObject): ModelParameters {
  if (!record(model)) return unknown();
  const card = record(model.cardData) ? model.cardData : {};
  const names = [
    ['repository name', model.id],
    ['repository name', model.modelId],
    ['repository name', model.repo_id],
    ['model name', model.name],
    ['model-card name', card.model_name],
  ] as const;
  for (const [field, value] of names) {
    const name = modelName(value);
    if (!name) continue;
    const sizes = namedSizes(name);
    if (sizes.length > 1) return unknown();
    if (sizes.length === 1) return describe(sizes[0], `${field} "${name}"`);
  }
  const bases = [...baseModels(card.base_model), ...baseModels(model.base_model)];
  if (!bases.length) return unknown();
  let chosen: NamedSize | undefined;
  let provenance = '';
  for (const base of bases) {
    const name = modelName(base);
    const sizes = name ? namedSizes(name) : [];
    // Multiple base models can describe a merge. A known constituent's size
    // cannot establish the result's size when another base is unknown/different.
    if (sizes.length !== 1 || (chosen && (chosen.count !== sizes[0].count || chosen.effective !== sizes[0].effective))) return unknown();
    chosen = sizes[0];
    if (!provenance) provenance = `base model name "${base}"`;
  }
  return chosen ? describe(chosen, provenance) : unknown();
}
