export function bytes(value = 0) {
  if (!Number.isFinite(value) || value <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const index = Math.min(4, Math.floor(Math.log(value) / Math.log(1024)));
  return `${(value / 1024 ** index).toFixed(index > 1 ? 1 : 0)} ${units[index]}`;
}
export function downloadState(download) { return download.state ?? download.status ?? 'queued'; }
export function downloadProgress(download) {
  if (download.total_bytes > 0) return Math.min(100, Math.max(0, download.downloaded_bytes / download.total_bytes * 100));
  return Math.min(100, Math.max(0, download.progress ?? 0));
}
export function mergeMessageDelta(messages, delta, reasoningDelta = '') {
  const last = messages.at(-1);
  if (last?.role === 'assistant') return [...messages.slice(0, -1), { ...last, content: last.content + delta, reasoning_content: (last.reasoning_content ?? '') + reasoningDelta }];
  return [...messages, { role: 'assistant', content: delta, reasoning_content: reasoningDelta }];
}
export function parseOptions(text) {
  const value = JSON.parse(text);
  if (!value || Array.isArray(value) || typeof value !== 'object') throw new Error('Use a JSON object.');
  return value;
}
export function parseEnvironment(text) {
  const result = {};
  for (const line of text.split('\n')) {
    if (!line.trim() || line.trim().startsWith('#')) continue;
    const split = line.indexOf('=');
    if (split < 1 || !/^[A-Za-z_][A-Za-z0-9_]*$/.test(line.slice(0, split).trim())) throw new Error('Environment variables must use NAME=value, one per line.');
    result[line.slice(0, split).trim()] = line.slice(split + 1);
  }
  return result;
}
export function modelMetadata(info) {
  const files = Array.isArray(info.siblings) ? info.siblings.filter(file => file && typeof file === 'object') : [];
  const weights = files.filter(file => /\.safetensors$/i.test(file.rfilename ?? file.path ?? ''));
  const validSize = size => Number.isSafeInteger(size) && size >= 0;
  const sizeOf = file => validSize(file.size) ? file.size : validSize(file.lfs?.size) ? file.lfs.size : null;
  const sizes = weights.map(sizeOf);
  const knownWeights = sizes.filter(size => size != null);
  const rawWeightsBytes = knownWeights.reduce((sum, size) => sum + size, 0);
  const summary = info.studio_size ?? {};
  const summarizedWeights = validSize(summary.weights_bytes) && summary.weights_bytes > 0;
  const weightsBytes = summarizedWeights ? summary.weights_bytes : Number.isSafeInteger(rawWeightsBytes) && rawWeightsBytes > 0 ? rawWeightsBytes : null;
  const complete = weightsBytes != null && (typeof summary.weights_complete === 'boolean'
    ? summary.weights_complete
    : weights.length > 0 && sizes.every(size => size != null && size > 0));
  const downloadBytes = validSize(summary.download_bytes) && summary.download_bytes > 0 ? summary.download_bytes : null;
  const downloadComplete = downloadBytes != null && summary.download_complete === true;
  const name = info.id ?? info.modelId ?? '';
  const quantization = info.cardData?.quantization ?? (info.tags ?? []).find(tag => /^\d+[- ]?bit$|^q[248]/i.test(tag)) ?? name.match(/\b\d+[- ]?bit\b|\bbf16\b|\bfp16\b/i)?.[0] ?? null;
  return { files, weights, weightsBytes, sizeComplete: complete, downloadBytes, downloadComplete, revision: summary.revision ?? info.sha ?? null, quantization };
}
// Discover sizes use decimal GB. Parameter counts and Hub-wide storage are never file sizes.
export function modelSizeLabel(value, complete = true) {
  if (!Number.isSafeInteger(value) || value <= 0) return 'Size unavailable';
  const gigabytes = value / 1_000_000_000;
  const digits = gigabytes < 0.01 ? 9 : 2;
  const scale = 10 ** digits;
  const amount = complete ? gigabytes : Math.floor(gigabytes * scale) / scale;
  const formatted = Intl.NumberFormat('en', { minimumFractionDigits: 2, maximumFractionDigits: digits }).format(amount);
  return `${complete ? '' : '≥ '}${formatted} GB`;
}
export function logLines(value) {
  if (value.text != null) return value.text.split('\n');
  return (Array.isArray(value) ? value : value.lines ?? value.logs ?? [])
    .map(line => typeof line === 'string' ? line : line.line ?? JSON.stringify(line));
}


export function logEntries(value) {
  const structured = Array.isArray(value?.entries) ? value.entries : null;
  const source = structured ?? (value?.text === '' ? [] : logLines(value ?? {}));
  return source.map((entry, index) => {
    const line = typeof entry === 'string' ? entry : typeof entry?.line === 'string' ? entry.line : JSON.stringify(entry) ?? '';
    const rawTimestamp = structured && typeof entry?.timestamp === 'string' ? entry.timestamp : null;
    const day = rawTimestamp?.slice(0, 10);
    const dayTime = day ? Date.parse(`${day}T00:00:00Z`) : NaN;
    const validDay = Number.isFinite(dayTime) && new Date(dayTime).toISOString().slice(0, 10) === day;
    const time = rawTimestamp && validDay && /^\d{4}-\d{2}-\d{2}T(?:[01]\d|2[0-3]):[0-5]\d:[0-5]\d(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$/.test(rawTimestamp) ? Date.parse(rawTimestamp) : NaN;
    return { number: index + 1, line, timestamp: Number.isFinite(time) ? new Date(time).toISOString() : null };
  });
}
export function filterLogEntries(entries, query) {
  const term = query.trim().toLocaleLowerCase();
  if (!term) return entries;
  return entries.filter(entry => `${entry.line} ${entry.timestamp ?? ''} ${entry.timestamp?.slice(11, 23) ?? ''}`.toLocaleLowerCase().includes(term));
}
export function copyLogEntries(entries) {
  return entries.map(entry => entry.timestamp ? `[${entry.timestamp}] ${entry.line}` : entry.line).join('\n');
}
