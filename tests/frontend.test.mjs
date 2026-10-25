import assert from 'node:assert/strict';
import test from 'node:test';
import { bytes, downloadProgress, downloadState, mergeMessageDelta, parseEnvironment, parseOptions, modelMetadata, logLines } from '../src/utils.mjs';

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
