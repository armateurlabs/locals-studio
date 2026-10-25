import { spawnSync } from 'node:child_process';
import { chmodSync, copyFileSync, mkdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const release = process.argv[2] === 'release';
function run(program, args) {
  const result = spawnSync(program, args, { cwd: root, stdio: 'inherit', env: { ...process.env, CARGO_BUILD_JOBS: '1' } });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}
const rust = spawnSync('rustc', ['-vV'], { encoding: 'utf8' });
if (rust.status !== 0) throw new Error('Rust is required to build the service sidecar.');
const triple = rust.stdout.match(/^host: (.+)$/m)?.[1];
if (!triple) throw new Error('Cannot determine the native Rust target.');
if (process.platform !== 'darwin' || process.arch !== 'arm64') throw new Error('Locals Studio desktop targets macOS Apple Silicon.');
run('cargo', ['build', '--locked', '-p', 'studio-cli', ...(release ? ['--release'] : [])]);
const directory = resolve(root, 'src-tauri/binaries');
mkdirSync(directory, { recursive: true });
const destination = resolve(directory, `locals-studio-${triple}`);
copyFileSync(resolve(root, 'target', release ? 'release' : 'debug', 'locals-studio'), destination);
chmodSync(destination, 0o755);
console.log(`Prepared service sidecar: ${destination}`);
