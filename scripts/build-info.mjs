import { spawnSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');

function git(cwd, args) {
  const result = spawnSync('git', ['-C', cwd, ...args], {
    encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], timeout: 2_000,
  });
  return result.status === 0 ? result.stdout.trim() : null;
}

function commitId(value) {
  return typeof value === 'string' && /^(?:[a-f\d]{40}|[a-f\d]{64})$/i.test(value) ? value.toLowerCase() : null;
}

function tagName(value) {
  // Git refs contain no whitespace or control characters. Quotes and shell
  // metacharacters are legitimate data and remain untouched in the JSON output.
  return typeof value === 'string' && value.length && !/[\s\x00-\x1f\x7f]/.test(value) ? value : null;
}

/** Resolve a display identity without changing package or installer versions. */
export function resolveBuildInfo({ env = process.env, gitcwd = root } = {}) {
  const commit = commitId(git(gitcwd, ['rev-parse', '--verify', 'HEAD'])) ?? commitId(env.GITHUB_SHA);
  const refTag = typeof env.GITHUB_REF === 'string' && env.GITHUB_REF.startsWith('refs/tags/')
    ? env.GITHUB_REF.slice('refs/tags/'.length) : null;
  const eventTag = env.GITHUB_REF_TYPE === 'tag'
    ? tagName(env.GITHUB_REF_NAME) ?? tagName(refTag)
    : env.GITHUB_REF_TYPE == null || env.GITHUB_REF_TYPE === '' ? tagName(refTag) : null;
  // The event tag identifies the release being built when several tags point at
  // HEAD. Outside a tag event, choose an exact HEAD tag deterministically.
  const exactTags = git(gitcwd, ['tag', '--points-at', 'HEAD']);
  const tag = eventTag ?? exactTags?.split('\n').map(tagName).filter(Boolean).sort()[0] ?? null;
  return { label: tag ?? commit?.slice(0, 7) ?? 'unknown', commit, tag };
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.stdout.write(`${JSON.stringify(resolveBuildInfo())}\n`);
}
