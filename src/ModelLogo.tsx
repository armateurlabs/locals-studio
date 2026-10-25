import { useEffect, useState } from 'react';
import { Cpu } from 'lucide-react';
import { request } from './api';

interface Props { repoId?: string | null; name?: string; className?: string; size?: number; }
interface CachedLogo { promise: Promise<string | null>; expiresAt: number; }

const logos = new Map<string, CachedLogo>();
const queue: Array<() => void> = [];
let activeRequests = 0;
const SUCCESS_TTL = 7 * 24 * 60 * 60 * 1000;
const RETRY_TTL = 60 * 60 * 1000;
const MAX_DATA_URL_LENGTH = 350_000;
const MAX_CACHED_PUBLISHERS = 256;
const publisherOf = (repoId?: string | null) => typeof repoId === 'string' && /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repoId) ? repoId.split('/')[0].toLowerCase() : null;
const isRasterDataUrl = (value: unknown): value is string => typeof value === 'string' && value.length <= MAX_DATA_URL_LENGTH && /^data:image\/(png|jpeg|gif|webp);base64,[A-Za-z0-9+/]+={0,2}$/.test(value);
// Official Hugging Face organization avatars identify recognizable model families,
// including conversions published by a community account.
const families = [
  { match: /(?:^|[-_.\s])qwen(?:[-_.\s]|\d|$)/i, namespace: 'Qwen', label: 'Qwen' },
  { match: /(?:^|[-_.\s])deepseek(?:[-_.\s]|\d|$)/i, namespace: 'deepseek-ai', label: 'DeepSeek' },
  { match: /(?:^|[-_.\s])(?:mistral|mixtral)(?:[-_.\s]|\d|$)/i, namespace: 'mistralai', label: 'Mistral' },
  { match: /(?:^|[-_.\s])llama(?:[-_.\s]|\d|$)/i, namespace: 'meta-llama', label: 'Meta Llama' },
  { match: /(?:^|[-_.\s])gemma(?:[-_.\s]|\d|$)/i, namespace: 'google', label: 'Google' },
  { match: /(?:^|[-_.\s])phi(?:[-_.\s]|\d|$)/i, namespace: 'microsoft', label: 'Microsoft' },
];

function runQueue() {
  while (activeRequests < 4 && queue.length) queue.shift()!();
}

function trimCache() {
  for (const [publisher, cached] of logos) {
    if (logos.size <= MAX_CACHED_PUBLISHERS) break;
    if (Number.isFinite(cached.expiresAt)) logos.delete(publisher);
  }
}

function loadLogo(repoId: string, publisher: string) {
  const cached = logos.get(publisher);
  if (cached && cached.expiresAt > Date.now()) {
    logos.delete(publisher); logos.set(publisher, cached);
    return cached.promise;
  }
  let resolve!: (value: string | null) => void;
  const entry: CachedLogo = { promise: new Promise<string | null>(done => { resolve = done; }), expiresAt: Infinity };
  logos.set(publisher, entry);
  queue.push(() => {
    activeRequests++;
    void request<{ data_url?: string | null }>('model.logo', { repo_id: repoId })
      .then(result => isRasterDataUrl(result?.data_url) ? result.data_url : null)
      .catch(() => null)
      .then(value => { entry.expiresAt = Date.now() + (value ? SUCCESS_TTL : RETRY_TTL); resolve(value); trimCache(); })
      .finally(() => { activeRequests--; runQueue(); });
  });
  runQueue();
  return entry.promise;
}

export default function ModelLogo({ repoId, name, className = '', size = 24 }: Props) {
  const publisher = publisherOf(repoId);
  const family = families.find(family => family.match.test(`${repoId?.split('/').at(-1) ?? ''} ${name ?? ''}`));
  const candidates = [
    ...(family ? [{ publisher: family.namespace.toLowerCase(), repoId: `${family.namespace}/avatar`, label: `${family.label} model family` }] : []),
    ...(publisher && publisher !== family?.namespace.toLowerCase() ? [{ publisher, repoId: repoId!, label: `${publisher} publisher` }] : []),
  ];
  const identity = candidates.map(candidate => candidate.publisher).join('|');
  const [loaded, setLoaded] = useState<{ identity: string; publisher: string; label: string; src: string } | null>(null);
  const [attempt, setAttempt] = useState(0);
  const src = loaded?.identity === identity ? loaded.src : null;
  useEffect(() => {
    let alive = true;
    if (!identity) { setLoaded(null); return; }
    void (async () => {
      for (const candidate of candidates) {
        const value = await loadLogo(candidate.repoId, candidate.publisher);
        if (!alive) return;
        if (value) { setLoaded({ identity, publisher: candidate.publisher, label: candidate.label, src: value }); return; }
      }
      setLoaded(null);
    })();
    return () => { alive = false; };
  }, [repoId, identity, attempt]);
  const failed = () => {
    if (loaded) logos.set(loaded.publisher, { promise: Promise.resolve(null), expiresAt: Date.now() + RETRY_TTL });
    trimCache();
    setLoaded(null);
    setAttempt(previous => previous + 1);
  };
  return <span className={`model-logo ${className}`} style={{ width: size, height: size }} aria-hidden="true" title={src && loaded ? `${loaded.label} avatar on Hugging Face` : undefined}>
    {src ? <img src={src} alt="" draggable={false} onError={failed} /> : <Cpu size={Math.max(12, Math.round(size * 0.6))} />}
  </span>;
}
