import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { lstatSync, mkdtempSync, readFileSync, readlinkSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const config = JSON.parse(readFileSync(join(root, 'src-tauri/tauri.conf.json'), 'utf8'));
const pkg = resolve(process.argv[2] ?? join(root, 'target/release/bundle/pkg', `${config.productName}_${config.version}_aarch64.pkg`));
const temporary = mkdtempSync(join(tmpdir(), 'locals-studio-pkg-verify-'));
function run(program, args) {
  const result = spawnSync(program, args, { encoding: 'utf8' });
  if (result.error) throw result.error;
  assert.equal(result.status, 0, `${program}: ${result.stderr}`);
  return result.stdout;
}
try {
  const expanded = join(temporary, 'expanded');
  run('/usr/sbin/pkgutil', ['--expand-full', pkg, expanded]);
  const component = join(expanded, 'locals-studio-component.pkg');
  const info = readFileSync(join(component, 'PackageInfo'), 'utf8');
  assert.match(info, /install-location="\/Applications"/);
  assert.ok(info.includes(`identifier="${config.identifier}.installer"`));
  assert.ok(info.includes(`version="${config.version}"`));
  assert.match(info, /relocatable="false"/, 'the app must stay at its fixed installation path');
  assert.match(info, /<relocate\s*\/>/, 'the package must not search for relocated app bundles');
  assert.ok(info.includes('./Locals Studio.app'));
  const cliComponent = join(expanded, 'locals-studio-cli.pkg');
  const pathComponent = join(expanded, 'locals-studio-path.pkg');
  for (const [directory, identifier, location] of [
    [cliComponent, `${config.identifier}.cli`, '/usr/local/bin'],
    [pathComponent, `${config.identifier}.path`, '/etc/paths.d'],
  ]) {
    const componentInfo = readFileSync(join(directory, 'PackageInfo'), 'utf8');
    assert.ok(componentInfo.includes(`identifier="${identifier}"`));
    assert.ok(componentInfo.includes(`install-location="${location}"`));
    assert.ok(componentInfo.includes(`version="${config.version}"`));
  }
  for (const directory of [component, cliComponent, pathComponent]) {
    const paths = run('/usr/bin/lsbom', ['-s', join(directory, 'Bom')]).trim().split('\n');
    assert.ok(!paths.some(path => /^\.\/(?:Applications|usr|etc)(?:\/|$)/.test(path)), 'shared ancestor directories must not be archived');
  }
  const link = join(cliComponent, 'Payload/locals-studio');
  assert.ok(lstatSync(link).isSymbolicLink());
  assert.equal(readlinkSync(link), '/Applications/Locals Studio.app/Contents/MacOS/locals-studio');
  assert.equal(readFileSync(join(pathComponent, 'Payload/locals-studio'), 'utf8'), '/usr/local/bin\n');
  const preinstall = join(component, 'Scripts/preinstall');
  assert.ok(lstatSync(preinstall).mode & 0o111, 'preinstall must be executable');
  assert.deepEqual(readFileSync(preinstall), readFileSync(join(root, 'scripts/macos-installer/preinstall')));
  const app = join(component, 'Payload/Locals Studio.app');
  run('/usr/bin/codesign', ['--verify', '--deep', '--strict', app]);
  assert.equal(run(join(app, 'Contents/MacOS/locals-studio'), ['--version']).trim(), `locals-studio ${config.version}`);
  const distribution = readFileSync(join(expanded, 'Distribution'), 'utf8');
  assert.match(distribution, /hostArchitectures="arm64"/);
  assert.ok(distribution.includes(`min="${config.bundle.macOS.minimumSystemVersion}"`));
  assert.match(distribution, /enable_currentUserHome="false"/);
  assert.match(distribution, /enable_localSystem="true"/);
  assert.match(distribution, /enable_anywhere="false"/);
  console.log(`Verified installer payload, fixed app path, CLI, PATH entry, preinstall, signature and platform requirements: ${pkg}`);
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
