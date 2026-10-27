import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { resolveBuildInfo } from '../scripts/build-info.mjs';

function git(cwd, ...args) {
  const result = spawnSync('git', ['-C', cwd, ...args], { encoding: 'utf8' });
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
  return result.stdout.trim();
}

function fixture(t, initialized = true) {
  const cwd = mkdtempSync(join(tmpdir(), 'locals build info-'));
  t.after(() => rmSync(cwd, { recursive: true, force: true }));
  if (initialized) {
    git(cwd, 'init', '--quiet');
    git(cwd, 'config', 'user.name', 'Build identity test');
    git(cwd, 'config', 'user.email', 'build-test@example.invalid');
    git(cwd, 'config', 'commit.gpgsign', 'false');
    git(cwd, 'config', 'tag.gpgsign', 'false');
    git(cwd, 'commit', '--allow-empty', '--quiet', '-m', 'Initial fixture');
  }
  return cwd;
}

// Real Git fixtures cover annotated/lightweight tags and exact-HEAD behavior,
// rather than mocking a command output that might differ from Git itself.
test('untagged builds use the actual checkout commit and a seven-character label', t => {
  const gitcwd = fixture(t);
  const commit = git(gitcwd, 'rev-parse', 'HEAD');
  const info = resolveBuildInfo({ gitcwd, env: { GITHUB_REF_TYPE: 'branch', GITHUB_REF_NAME: 'main', GITHUB_SHA: 'f'.repeat(40) } });
  assert.deepEqual(info, { label: commit.slice(0, 7), commit, tag: null });
});

test('local builds recognize lightweight and annotated tags only at HEAD', t => {
  const gitcwd = fixture(t);
  git(gitcwd, 'tag', 'v1-lightweight');
  assert.equal(resolveBuildInfo({ gitcwd, env: {} }).label, 'v1-lightweight');
  git(gitcwd, 'commit', '--allow-empty', '--quiet', '-m', 'Next fixture');
  const commit = git(gitcwd, 'rev-parse', 'HEAD');
  assert.deepEqual(resolveBuildInfo({ gitcwd, env: {} }), { label: commit.slice(0, 7), commit, tag: null });
  git(gitcwd, 'tag', '-a', 'v2-annotated', '-m', 'Annotated fixture');
  assert.deepEqual(resolveBuildInfo({ gitcwd, env: {} }), { label: 'v2-annotated', commit, tag: 'v2-annotated' });
});

test('the GitHub event tag wins when multiple tags point at the checked-out commit', t => {
  const gitcwd = fixture(t);
  git(gitcwd, 'tag', 'a-other-release');
  git(gitcwd, 'tag', 'v2-requested-release');
  const commit = git(gitcwd, 'rev-parse', 'HEAD');
  const env = { GITHUB_REF_TYPE: 'tag', GITHUB_REF_NAME: 'v2-requested-release', GITHUB_REF: 'refs/tags/v2-requested-release', GITHUB_SHA: commit };
  assert.deepEqual(resolveBuildInfo({ gitcwd, env }), { label: 'v2-requested-release', commit, tag: 'v2-requested-release' });
  assert.equal(resolveBuildInfo({ gitcwd, env: {} }).tag, 'a-other-release', 'the non-event choice is deterministic');
  assert.equal(resolveBuildInfo({ gitcwd, env: { GITHUB_REF_TYPE: 'branch', GITHUB_REF_NAME: 'main' } }).tag, 'a-other-release', 'a branch name is never used as a tag');
});

test('GitHub tag and SHA context work in source archives without a Git checkout', t => {
  const gitcwd = fixture(t, false);
  const commit = 'ABCDEF0123456789ABCDEF0123456789ABCDEF01';
  assert.deepEqual(resolveBuildInfo({ gitcwd, env: { GITHUB_REF: 'refs/tags/release/v3', GITHUB_SHA: commit } }), {
    label: 'release/v3', commit: commit.toLowerCase(), tag: 'release/v3',
  });
  assert.deepEqual(resolveBuildInfo({ gitcwd, env: { GITHUB_SHA: commit, GITHUB_REF_TYPE: 'branch', GITHUB_REF_NAME: 'main' } }), {
    label: 'abcdef0', commit: commit.toLowerCase(), tag: null,
  });
});

test('missing Git metadata has an honest fallback and ignores malformed SHA or tag values', t => {
  const gitcwd = fixture(t, false);
  assert.deepEqual(resolveBuildInfo({ gitcwd, env: {} }), { label: 'unknown', commit: null, tag: null });
  assert.deepEqual(resolveBuildInfo({ gitcwd, env: { GITHUB_SHA: 'not-a-commit', GITHUB_REF_TYPE: 'tag', GITHUB_REF_NAME: 'bad\nlabel' } }), {
    label: 'unknown', commit: null, tag: null,
  });
});

test('tag quotes and shell metacharacters remain literal data and unrelated environment secrets are excluded', t => {
  const gitcwd = fixture(t);
  const tag = 'release/"$HOME";$(printf)';
  git(gitcwd, 'tag', tag);
  const commit = git(gitcwd, 'rev-parse', 'HEAD');
  const info = resolveBuildInfo({ gitcwd, env: { GITHUB_REF_TYPE: 'tag', GITHUB_REF_NAME: tag, GITHUB_SHA: commit, DEPLOY_SECRET: 'must-never-appear' } });
  assert.deepEqual(info, { label: tag, commit, tag });
  const json = JSON.stringify(info);
  assert.deepEqual(JSON.parse(json), info);
  assert.doesNotMatch(json, /must-never-appear|DEPLOY_SECRET/);
  assert.equal(resolveBuildInfo({ gitcwd, env: {} }).tag, tag, 'Git arguments are not evaluated by a shell either');
});
