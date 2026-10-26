import assert from 'node:assert/strict';
import test from 'node:test';
import { bytes, downloadProgress, downloadState, mergeMessageDelta, parseEnvironment, parseOptions, modelMetadata, modelSizeLabel, logLines, logEntries, filterLogEntries, copyLogEntries } from '../src/utils.mjs';

test('streaming deltas append without changing the original conversation', () => {
  const before = [{ role: 'user', content: 'Hello' }];
  const first = mergeMessageDelta(before, 'Good');
  const second = mergeMessageDelta(first, ' morning');
  assert.equal(before.length, 1);
  assert.equal(first[1].content, 'Good');
  assert.equal(second[1].content, 'Good morning');
  assert.equal(second.filter(message => message.role === 'assistant').length, 1);
});

test('download progress uses durable byte counts and stays in range', () => {
  assert.equal(downloadProgress({ downloaded_bytes: 50, total_bytes: 100 }), 50);
  assert.equal(downloadProgress({ downloaded_bytes: 120, total_bytes: 100 }), 100);
  assert.equal(downloadProgress({ progress: -5 }), 0);
  assert.equal(downloadState({ status: 'paused' }), 'paused');
});

test('environment editor preserves equals signs in values and rejects invalid names', () => {
  assert.deepEqual(parseEnvironment('# local settings\nLOCALS_KV_BITS=8\nTOKEN=a=b\n'), { LOCALS_KV_BITS: '8', TOKEN: 'a=b' });
  assert.throws(() => parseEnvironment('not valid=value'));
  assert.throws(() => parseEnvironment('VALUE'));
});

test('advanced options require an object and byte display handles missing data', () => {
  assert.deepEqual(parseOptions('{"slots":1}'), { slots: 1 });
  for (const value of ['null', '[]', 'true', '123']) assert.throws(() => parseOptions(value));
  assert.equal(bytes(1024 ** 3), '1.0 GB');
  assert.equal(bytes(undefined), '0 B');
});

test('model metadata does not turn an incomplete Hub listing into a complete size', () => {
  const info = { id: 'mlx-community/Test-4bit', siblings: [{ rfilename: 'config.json', size: 3 }, { rfilename: 'model-00001.safetensors', size: 100 }, { rfilename: 'model-00002.safetensors', lfs: { size: 200 } }] };
  assert.equal(modelMetadata(info).weightsBytes, 300);
  assert.equal(modelMetadata(info).sizeComplete, true);
  assert.equal(modelMetadata(info).quantization, '4bit');
  info.siblings.push({ rfilename: 'model-00003.safetensors' });
  assert.equal(modelMetadata(info).sizeComplete, false);
});

test('Discover separates parameter counts, repository storage, weights and download bytes', () => {
  const unknown = modelMetadata({ safetensors: { total: 8_000_000_000 }, usedStorage: 50_000_000_000 });
  assert.equal(unknown.weightsBytes, null);
  assert.equal(unknown.downloadBytes, null);
  assert.equal(modelSizeLabel(unknown.weightsBytes), 'Size unavailable');
  const sized = modelMetadata({ sha: 'commit', studio_size: {
    revision: 'pinned', weights_bytes: 4_250_000_000, weights_complete: true,
    download_bytes: 4_300_000_000, download_complete: true,
    repository_bytes: 50_000_000_000, hub_storage_bytes: 100_000_000_000,
  } });
  assert.equal(sized.weightsBytes, 4_250_000_000);
  assert.equal(sized.downloadBytes, 4_300_000_000);
  assert.equal(sized.revision, 'pinned');
  assert.equal(modelSizeLabel(sized.weightsBytes, sized.sizeComplete), '4.25 GB');
  assert.equal(modelSizeLabel(sized.downloadBytes, sized.downloadComplete), '4.30 GB');
  const partial = modelMetadata({ siblings: [{ rfilename: 'model.safetensors', size: 100 }], studio_size: { weights_bytes: 100, weights_complete: false } });
  assert.equal(partial.sizeComplete, false, 'an explicitly partial Hub summary stays partial');
});

test('Discover preserves lower bounds and never labels missing or invalid sizes as zero GB', () => {
  const partial = modelMetadata({ siblings: [
    { rfilename: 'model-1.safetensors', size: -1, lfs: { size: 1_255_000_000 } },
    { rfilename: 'model-2.safetensors', size: '2000' },
    { rfilename: 'tokenizer.json', size: 500 },
  ] });
  assert.equal(partial.weightsBytes, 1_255_000_000);
  assert.equal(partial.sizeComplete, false);
  assert.equal(modelSizeLabel(partial.weightsBytes, partial.sizeComplete), '≥ 1.25 GB');
  assert.equal(partial.downloadBytes, null, 'repository files alone do not assert a Studio download total');
  for (const invalid of [null, undefined, 0, -1, NaN, Infinity, '1000000000']) {
    assert.equal(modelSizeLabel(invalid), 'Size unavailable');
  }
  assert.equal(modelSizeLabel(1_000_000_000), '1.00 GB', 'GB is decimal, not GiB');
  assert.equal(modelSizeLabel(100), '0.0000001 GB', 'a tiny known file is not rounded to zero');
});

test('reasoning deltas remain separate from the visible answer', () => {
  const first = mergeMessageDelta([{ role: 'user', content: 'Why?' }], '', 'Considering');
  const next = mergeMessageDelta(first, 'Because.', ' options.');
  assert.equal(next[1].content, 'Because.');
  assert.equal(next[1].reasoning_content, 'Considering options.');
});

test('server logs consume the service text response and preserve blank lines', () => {
  assert.deepEqual(logLines({ text: 'Starting\n\nReady', path: '/tmp/server.log', truncated: false }), ['Starting', '', 'Ready']);
  assert.deepEqual(logLines({ text: '' }), ['']);
  assert.deepEqual(logLines({ lines: [{ line: 'Ready' }] }), ['Ready']);
});


test('structured log entries preserve real timestamps and legacy lines remain untimed', () => {
  const entries = logEntries({ text: 'fallback is ignored', entries: [
    { line: 'Started', timestamp: '2026-10-26T13:22:23.123+02:00' },
    { line: 'Legacy line', timestamp: null },
    { line: 'Invalid date', timestamp: '2026-02-30T11:22:23.123Z' },
    { line: 'Invalid clock', timestamp: '2026-10-26T24:22:23.123Z' },
    { line: 'Timestamp type is invalid', timestamp: 123 },
  ] });
  assert.deepEqual(entries.map(entry => entry.number), [1, 2, 3, 4, 5]);
  assert.equal(entries[0].timestamp, '2026-10-26T11:22:23.123Z');
  assert.ok(entries.slice(1).every(entry => entry.timestamp === null));
  assert.deepEqual(logEntries({ text: 'Old\n\nReady' }), [{ number: 1, line: 'Old', timestamp: null }, { number: 2, line: '', timestamp: null }, { number: 3, line: 'Ready', timestamp: null }]);
  assert.deepEqual(logEntries({ text: '', entries: [] }), []);
  assert.deepEqual(logEntries({ text: '' }), []);
});

test('log filtering keeps original line numbers and copies only the matching lines with known times', () => {
  const entries = logEntries({ entries: [
    { line: 'Started', timestamp: '2026-10-26T11:22:22.000Z' },
    { line: 'Legacy ERROR', timestamp: null },
    { line: 'Error: failed request', timestamp: '2026-10-26T11:22:23.123Z' },
  ] });
  const errors = filterLogEntries(entries, ' eRrOr ');
  assert.deepEqual(errors.map(entry => entry.number), [2, 3]);
  assert.equal(copyLogEntries(errors), 'Legacy ERROR\n[2026-10-26T11:22:23.123Z] Error: failed request');
  assert.deepEqual(filterLogEntries(entries, '11:22:23.123').map(entry => entry.number), [3]);
  assert.deepEqual(filterLogEntries(entries, '2026-10-26T11:22:22').map(entry => entry.number), [1]);
  assert.equal(filterLogEntries(entries, ''), entries);
  assert.deepEqual(filterLogEntries(entries, 'unmatched'), []);
  assert.equal(copyLogEntries([]), '');
});
