import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { lstatSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, readlinkSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const script = resolve(dirname(fileURLToPath(import.meta.url)), '../scripts/macos-installer/preinstall');
const ownCliTarget = '/Applications/Locals Studio.app/Contents/MacOS/locals-studio';
const onlyMac = { skip: process.platform !== 'darwin' };

test('installer builds reject flags that could package an old release app', () => {
  const builder = resolve(dirname(fileURLToPath(import.meta.url)), '../scripts/build-desktop.mjs');
  for (const args of [['--debug'], ['--target', 'aarch64-apple-darwin'], ['--no-bundle'], ['--config=override.json']]) {
    const result = spawnSync(process.execPath, [builder, ...args], { encoding: 'utf8', timeout: 5_000 });
    assert.ifError(result.error);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /Output-changing Tauri flags are not supported/);
  }
});

function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), 'locals installer-'));
  const volume = join(root, 'destination volume with spaces');
  mkdirSync(volume);
  t.after(() => rmSync(root, { recursive: true, force: true }));
  return { root, volume };
}
function file(volume, path, data) {
  const target = join(volume, path);
  mkdirSync(dirname(target), { recursive: true });
  writeFileSync(target, data);
  return target;
}
function link(volume, path, destination) {
  const target = join(volume, path);
  mkdirSync(dirname(target), { recursive: true });
  symlinkSync(destination, target);
  return target;
}
function app(volume, identifier = 'com.locals.studio') {
  const value = identifier == null ? '' : `<key>CFBundleIdentifier</key><string>${identifier}</string>`;
  return file(volume, 'Applications/Locals Studio.app/Contents/Info.plist', `<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0"><dict>${value}</dict></plist>\n`);
}
function tree(root) {
  const entries = [];
  function visit(directory, prefix = '') {
    for (const name of readdirSync(directory).sort()) {
      const path = join(directory, name);
      const relative = prefix ? `${prefix}/${name}` : name;
      const stat = lstatSync(path);
      if (stat.isSymbolicLink()) entries.push([relative, 'link', stat.mode, readlinkSync(path)]);
      else if (stat.isDirectory()) { entries.push([relative, 'directory', stat.mode]); visit(path, relative); }
      else entries.push([relative, 'file', stat.mode, readFileSync(path).toString('base64')]);
    }
  }
  visit(root);
  return entries;
}
function check(volume, expectedSuccess, expectedMessage) {
  const before = tree(volume);
  const result = spawnSync('/bin/sh', [script, '/package path/Locals Studio.pkg', '/ignored installation location', volume], { encoding: 'utf8', timeout: 5_000 });
  assert.ifError(result.error);
  assert.equal(result.status, expectedSuccess ? 0 : 1, result.stderr);
  if (expectedMessage) assert.match(result.stderr, expectedMessage);
  assert.deepEqual(tree(volume), before, 'preinstall only inspects the destination; it creates, removes and changes nothing');
}

test('preinstall is executable, valid POSIX shell, and accepts an empty destination volume', onlyMac, t => {
  assert.ok(lstatSync(script).mode & 0o111);
  const syntax = spawnSync('/bin/sh', ['-n', script], { encoding: 'utf8' });
  assert.equal(syntax.status, 0, syntax.stderr);
  const { volume } = fixture(t);
  check(volume, true);
  check(`${volume}/`, true);
});

test('preinstall permits upgrades only for its own app, exact CLI symlink and exact PATH entry', onlyMac, t => {
  const { volume } = fixture(t);
  app(volume);
  file(volume, 'Applications/Locals Studio.app/Contents/MacOS/locals-studio', 'existing application payload');
  link(volume, 'usr/local/bin/locals-studio', ownCliTarget);
  file(volume, 'etc/paths.d/locals-studio', '/usr/local/bin\n');
  file(volume, 'keep unrelated user data.txt', 'unchanged');
  check(volume, true);
});

test('preinstall permits repairing its own exact CLI symlink when the app is absent', onlyMac, t => {
  const { volume } = fixture(t);
  link(volume, 'usr/local/bin/locals-studio', ownCliTarget);
  check(volume, true);
});

for (const [name, prepare] of [
  ['regular file', volume => file(volume, 'usr/local/bin/locals-studio', 'another tool')],
  ['directory', volume => mkdirSync(join(volume, 'usr/local/bin/locals-studio'), { recursive: true })],
  ['unrelated live symlink', volume => { const other = file(volume, 'another CLI', 'another tool'); link(volume, 'usr/local/bin/locals-studio', other); }],
  ['dangling symlink', volume => link(volume, 'usr/local/bin/locals-studio', '/missing/unrelated-tool')],
  ['relative symlink', volume => link(volume, 'usr/local/bin/locals-studio', '../../../Applications/Locals Studio.app/Contents/MacOS/locals-studio')],
]) {
  test(`preinstall rejects a conflicting CLI ${name} without changing it`, onlyMac, t => {
    const { volume } = fixture(t);
    prepare(volume);
    check(volume, false, /locals-studio CLI link|\/usr\/local\/bin\/locals-studio/);
  });
}

for (const [name, prepare] of [
  ['different bundle identifier', volume => app(volume, 'org.example.unrelated')],
  ['missing bundle identifier', volume => app(volume, null)],
  ['invalid property list', volume => file(volume, 'Applications/Locals Studio.app/Contents/Info.plist', 'not a plist')],
  ['missing property list', volume => mkdirSync(join(volume, 'Applications/Locals Studio.app'), { recursive: true })],
  ['regular file', volume => file(volume, 'Applications/Locals Studio.app', 'keep this file')],
  ['symbolic link', volume => { const other = join(volume, 'other app'); mkdirSync(other); link(volume, 'Applications/Locals Studio.app', other); }],
  ['dangling symbolic link', volume => link(volume, 'Applications/Locals Studio.app', '/missing/Locals Studio.app')],
]) {
  test(`preinstall protects an existing app with a ${name}`, onlyMac, t => {
    const { volume } = fixture(t);
    prepare(volume);
    check(volume, false, /application|Applications\/Locals Studio.app/);
  });
}

for (const [name, prepare] of [
  ['different contents', volume => file(volume, 'etc/paths.d/locals-studio', '/another/bin\n')],
  ['extra newline', volume => file(volume, 'etc/paths.d/locals-studio', '/usr/local/bin\n\n')],
  ['missing newline', volume => file(volume, 'etc/paths.d/locals-studio', '/usr/local/bin')],
  ['directory', volume => mkdirSync(join(volume, 'etc/paths.d/locals-studio'), { recursive: true })],
  ['symlink to matching contents', volume => { const other = file(volume, 'other PATH entry', '/usr/local/bin\n'); link(volume, 'etc/paths.d/locals-studio', other); }],
  ['dangling symlink', volume => link(volume, 'etc/paths.d/locals-studio', '/missing/PATH-entry')],
]) {
  test(`preinstall rejects a PATH entry with ${name}`, onlyMac, t => {
    const { volume } = fixture(t);
    prepare(volume);
    check(volume, false, /\/etc\/paths.d\/locals-studio/);
  });
}

test('preinstall takes the destination from argument three and does not inspect argument two', onlyMac, t => {
  const { volume, root } = fixture(t);
  const unrelated = join(root, 'other installation location');
  mkdirSync(unrelated);
  file(unrelated, 'usr/local/bin/locals-studio', 'unrelated command');
  const before = tree(root);
  const result = spawnSync('/bin/sh', [script, '/package path/Locals Studio.pkg', unrelated, volume], { encoding: 'utf8', timeout: 5_000 });
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(tree(root), before);
});

test('preinstall rejects a relative destination before inspecting files', onlyMac, () => {
  const result = spawnSync('/bin/sh', [script, '/package.pkg', '/', 'relative destination'], { encoding: 'utf8', timeout: 5_000 });
  assert.ifError(result.error);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /absolute path/);
});
