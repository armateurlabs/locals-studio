import { spawnSync } from 'node:child_process';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
if (args.some(argument => /^(?:--(?:debug|target|no-bundle|config)(?:=|$)|-[dtc]$)/.test(argument))) {
  throw new Error('The installer build uses the native release app. Output-changing Tauri flags are not supported.');
}
function run(program, args) {
  const result = spawnSync(program, args, { cwd: root, stdio: 'inherit', env: { ...process.env, CARGO_BUILD_JOBS: '1' } });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}

run(process.execPath, [join(root, 'scripts/prepare-sidecar.mjs'), 'release']);
// npm forwards its trailing arguments here; keep --ci and --locked on the Tauri build.
run(join(root, 'node_modules/.bin/tauri'), ['build', '--features', 'custom-protocol', ...args]);
run(process.execPath, [join(root, 'scripts/build-macos-pkg.mjs')]);
run(process.execPath, [join(root, 'scripts/verify-macos-pkg.mjs')]);
