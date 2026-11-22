// Fetch the pinned Sparkle 2 release: the framework the app bundles and the tools that sign updates.
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const SPARKLE_VERSION = '2.10.1';
const SPARKLE_SHA256 = '1a856d2a9edb8e628de88121d7be04cf0e2167f8641650b78fca00b14244cf27';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const vendor = join(root, 'vendors/sparkle', SPARKLE_VERSION);
const framework = join(root, 'src-tauri/Frameworks/Sparkle.framework');
const stamp = join(root, 'src-tauri/Frameworks/.sparkle-version');

function run(program, args) {
  const result = spawnSync(program, args, { stdio: 'inherit' });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${program} ${args.join(' ')} failed`);
}

export async function prepareSparkle() {
  if (!existsSync(join(vendor, 'bin/sign_update'))) {
    const response = await fetch(`https://github.com/sparkle-project/Sparkle/releases/download/${SPARKLE_VERSION}/Sparkle-${SPARKLE_VERSION}.tar.xz`);
    if (!response.ok) throw new Error(`Could not download Sparkle ${SPARKLE_VERSION}: HTTP ${response.status}`);
    const archive = Buffer.from(await response.arrayBuffer());
    const digest = createHash('sha256').update(archive).digest('hex');
    if (digest !== SPARKLE_SHA256) throw new Error(`Sparkle ${SPARKLE_VERSION} does not match its pinned SHA-256 (${digest})`);
    rmSync(vendor, { recursive: true, force: true });
    mkdirSync(vendor, { recursive: true });
    const file = join(vendor, 'Sparkle.tar.xz');
    writeFileSync(file, archive);
    run('/usr/bin/tar', ['-xf', file, '-C', vendor]);
    rmSync(file);
  }
  const current = existsSync(stamp) ? readFileSync(stamp, 'utf8').trim() : '';
  if (current !== SPARKLE_VERSION || !existsSync(join(framework, 'Versions/B/Sparkle'))) {
    rmSync(framework, { recursive: true, force: true });
    mkdirSync(dirname(framework), { recursive: true });
    // ditto keeps the framework's symlinks and code signatures intact.
    run('/usr/bin/ditto', [join(vendor, 'Sparkle.framework'), framework]);
    writeFileSync(stamp, `${SPARKLE_VERSION}\n`);
  }
  return { framework, bin: join(vendor, 'bin') };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const { framework } = await prepareSparkle();
  console.log(`Prepared Sparkle ${SPARKLE_VERSION}: ${framework}`);
}
