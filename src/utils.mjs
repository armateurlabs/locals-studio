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
  const files = info.siblings ?? [];
  const weights = files.filter(file => /\.safetensors$/.test(file.rfilename ?? file.path ?? ''));
  const sizeOf = file => file.size ?? file.lfs?.size;
  const sizes = weights.map(sizeOf);
  const complete = weights.length > 0 && sizes.every(size => Number.isFinite(size) && size >= 0);
  const weightsBytes = sizes.reduce((sum, size) => sum + (Number.isFinite(size) ? size : 0), 0);
  const name = info.id ?? info.modelId ?? '';
  const quantization = info.cardData?.quantization ?? (info.tags ?? []).find(tag => /^\d+[- ]?bit$|^q[248]/i.test(tag)) ?? name.match(/\b\d+[- ]?bit\b|\bbf16\b|\bfp16\b/i)?.[0] ?? null;
  return { files, weights, weightsBytes, sizeComplete: complete, quantization };
}
export function logLines(value) {
  if (value.text != null) return value.text.split('\n');
  return (Array.isArray(value) ? value : value.lines ?? value.logs ?? [])
    .map(line => typeof line === 'string' ? line : line.line ?? JSON.stringify(line));
}
