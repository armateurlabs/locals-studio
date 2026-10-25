import assert from 'node:assert/strict';
import test from 'node:test';
import { modelParameters } from '../src/model-parameters';

test('published repository sizes produce compact parameter labels independent of quantization', () => {
  for (const [name, count, label] of [
    ['Qwen3.5-2B-MLX-4bit', 2e9, '2B'],
    ['Qwen3.5-4B-MLX-8bit', 4e9, '4B'],
    ['gemma-2-9b-it-2bit', 9e9, '9B'],
    ['Model-37B-fp16', 37e9, '37B'],
    ['Model-0.8B-4bit', 0.8e9, '0.8B'],
    ['Model-135M-8bit', 135e6, '135M'],
  ] as const) {
    const result = modelParameters({ id: `publisher/${name}` });
    assert.equal(result.count, count, name);
    assert.equal(result.label, label, name);
    assert.equal(result.estimated, true);
    assert.match(result.source!, /Published nominal parameter size from repository name/);
    assert.match(result.source!, /not an exact tensor count/);
    assert.doesNotMatch(result.label, /GB/);
  }
});

test('MoE active-expert sizes are not confused with published total sizes', () => {
  for (const name of ['Qwen3-35B-A3B-4bit', 'Model-35B-active-3B', 'Model-A3B-35B']) {
    assert.equal(modelParameters({ id: `publisher/${name}` }).count, 35e9, name);
  }
  for (const name of ['Model-A3B-4bit', 'Model-active-3B', 'Model-3B-active', 'Mixtral-8x7B-4bit']) {
    assert.equal(modelParameters({ id: `publisher/${name}` }).count, null, name);
  }
});

test('E2B and E4B retain the effective-size qualifier without inventing total parameters', () => {
  for (const amount of [2, 4]) {
    const result = modelParameters({ id: `mlx-community/gemma-3n-E${amount}B-it-4bit`, safetensors: { total: 999_999_999 } });
    assert.equal(result.count, amount * 1e9);
    assert.equal(result.label, `${amount}B effective`);
    assert.equal(result.estimated, true);
    assert.match(result.source!, /effective size/);
    assert.match(result.source!, /not the model's total parameter count/);
  }
});

test('names and documented base-model IDs provide fallback provenance', () => {
  const named = modelParameters({ id: 'publisher/Unnamed-conversion', name: 'Custom Model 4B' });
  assert.equal(named.count, 4e9);
  assert.match(named.source!, /model name "Custom Model 4B"/);
  const base = modelParameters({ id: 'publisher/Quantized', cardData: { base_model: 'original/Model-9B' } });
  assert.equal(base.count, 9e9);
  assert.match(base.source!, /base model name "original\/Model-9B"/);
  assert.equal(modelParameters({ modelId: 'publisher/Model-2B' }).count, 2e9);
  assert.equal(modelParameters({ repo_id: 'publisher/Model-37B' }).count, 37e9);
  assert.equal(modelParameters({ cardData: { model_name: 'Model 135M' } }).count, 135e6);
  assert.equal(modelParameters({ base_model: 'original/Model-0.8B' }).count, 0.8e9);
  assert.equal(modelParameters({ cardData: { base_model: ['original/Model-4B', 'other/Model-4B'] } }).count, 4e9);
});

test('current nominal names take priority over base models and packed tensor metadata', () => {
  const model = Object.freeze({
    id: 'mlx-community/Model-4B-4bit',
    cardData: Object.freeze({ base_model: 'publisher/Model-9B' }),
    safetensors: Object.freeze({ total: 500_000_000, parameters: Object.freeze({ U32: 500_000_000 }) }),
    studio_size: Object.freeze({ weights_bytes: 2e9, download_bytes: 2.1e9 }),
  });
  assert.equal(modelParameters(model).count, 4e9);
  assert.equal(model.safetensors.total, 500_000_000);
});

test('publisher names, bit precision, byte sizes, and version numbers never become parameter counts', () => {
  for (const model of [
    { id: 'publisher-37B/Unknown-model' },
    { id: 'publisher/Unknown-model', author: 'publisher-4B' },
    { id: 'publisher/Model-4bit' },
    { id: 'publisher/Model-8-bit' },
    { id: 'publisher/Model-bf16-fp16-Q4_0' },
    { id: 'publisher/Model-4GB-2GiB' },
    { id: 'publisher/Model-4B-bytes' },
    { id: 'publisher/Model-v2.0' },
    { id: 'publisher/Model-v4B' },
    { id: 'publisher/Model-1.2.3B' },
    { id: 'publisher/Model-1e3B' },
  ]) {
    assert.deepEqual(modelParameters(model), { count: null, label: 'Unknown', source: null, estimated: false }, JSON.stringify(model));
  }
});

test('missing or ambiguous metadata stays unknown instead of using tensor counts or file bytes', () => {
  for (const model of [
    {},
    { id: 'publisher/Unknown', safetensors: { total: 37e9, parameters: { F16: 37e9 } } },
    { id: 'publisher/Unknown', siblings: [{ rfilename: 'model.safetensors', size: 37e9 }] },
    { id: 'publisher/Unknown', usedStorage: 37e9, studio_size: { weights_bytes: 4e9, download_bytes: 4.1e9 } },
    { id: 'publisher/Unknown', parameter_count: 9e9, num_parameters: 9e9, parameters: 9e9 },
    { id: 'publisher/Merge-4B-9B', cardData: { base_model: 'original/Model-4B' } },
    { cardData: { base_model: ['original/Model-4B', 'original/Model-9B'] } },
    { cardData: { base_model: ['original/Model-4B', 'original/Unknown'] } },
  ]) assert.equal(modelParameters(model).count, null, JSON.stringify(model));
});

test('negative, zero, unsafe and malformed sizes remain unknown without throwing', () => {
  for (const name of ['-4B', 'Model--4B', 'Model-(-0.8B)', 'Model-0B', 'Model-999999999B', 'Model-InfinityB', 'Model-NaNB']) {
    assert.equal(modelParameters({ id: `publisher/${name}` }).count, null, name);
  }
  for (const model of [
    null, [], false, 42, 'Model-4B',
    { id: {}, modelId: [], repo_id: 4, name: true, cardData: null },
    { id: 'publisher/Unknown', cardData: { base_model: { id: 'publisher/Model-4B' }, model_name: 4 } },
    { cardData: { base_model: ['original/Model-4B', {}] } },
    { safetensors: { total: -4e9 }, parameter_count: -4e9 },
  ]) assert.doesNotThrow(() => assert.equal(modelParameters(model as any).count, null));
});
