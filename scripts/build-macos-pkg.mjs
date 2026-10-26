import { spawnSync } from 'node:child_process';
import { cpSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const config = JSON.parse(readFileSync(join(root, 'src-tauri/tauri.conf.json'), 'utf8'));
const manifest = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'));
if (process.platform !== 'darwin' || process.arch !== 'arm64') throw new Error('The installer targets macOS Apple Silicon.');
if (config.version !== manifest.version || !/^\d+\.\d+\.\d+$/.test(config.version)) throw new Error('Installer and app versions must match a numeric release version.');

function run(program, args, capture = false) {
  const result = spawnSync(program, args, { cwd: root, encoding: 'utf8', stdio: capture ? 'pipe' : 'inherit' });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${program} failed${capture ? `: ${result.stderr.trim()}` : '.'}`);
  return capture ? result.stdout.trim() : undefined;
}
function xml(value) { return String(value).replace(/[&<>"']/g, character => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&apos;' })[character]); }
function distributionElement(source, tag, content) {
  const existing = new RegExp(`<${tag}\\b[^>]*(?:/>|>[\\s\\S]*?</${tag}>)`);
  return existing.test(source) ? source.replace(existing, () => content) : source.replace(/(<installer-gui-script\b[^>]*>)/, (_, opening) => `${opening}\n    ${content}`);
}

const app = join(root, 'target/release/bundle/macos', `${config.productName}.app`);
const info = join(app, 'Contents/Info.plist');
if (run('/usr/bin/plutil', ['-extract', 'CFBundleIdentifier', 'raw', '-o', '-', info], true) !== config.identifier) throw new Error('The built application identifier does not match its configuration.');
if (run('/usr/bin/plutil', ['-extract', 'CFBundleShortVersionString', 'raw', '-o', '-', info], true) !== config.version) throw new Error('Build the current application before packaging the installer.');
run('/usr/bin/codesign', ['--verify', '--deep', '--strict', app]);

const outputDirectory = join(root, 'target/release/bundle/pkg');
mkdirSync(outputDirectory, { recursive: true });
const work = mkdtempSync(join(tmpdir(), 'locals-studio-installer-'));
const output = join(outputDirectory, `${config.productName}_${config.version}_aarch64.pkg`);
// Finish in a private temporary package before replacing an existing artifact.
const temporaryOutput = join(outputDirectory, `.locals-studio-${process.pid}.pkg`);
try {
  // Separate fixed install locations keep shared ancestor directories out of the
  // payload: installing a CLI must not reset /Applications or /usr/local permissions.
  const appPayload = join(work, 'app');
  const cliPayload = join(work, 'cli');
  const pathPayload = join(work, 'path');
  for (const directory of [appPayload, cliPayload, pathPayload]) mkdirSync(directory);
  cpSync(app, join(appPayload, `${config.productName}.app`), { recursive: true, dereference: false });
  symlinkSync('/Applications/Locals Studio.app/Contents/MacOS/locals-studio', join(cliPayload, 'locals-studio'));
  writeFileSync(join(pathPayload, 'locals-studio'), '/usr/local/bin\n', { mode: 0o644 });

  const components = join(work, 'components.plist');
  writeFileSync(components, `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><array><dict>
<key>RootRelativeBundlePath</key><string>${xml(config.productName)}.app</string>
<key>BundleIsRelocatable</key><false/>
<key>BundleHasStrictIdentifier</key><true/>
<key>BundleIsVersionChecked</key><true/>
<key>BundleOverwriteAction</key><string>upgrade</string>
</dict></array></plist>\n`);
  const component = join(work, 'locals-studio-component.pkg');
  run('/usr/bin/pkgbuild', [
    '--root', appPayload, '--component-plist', components,
    '--scripts', join(root, 'scripts/macos-installer'),
    '--identifier', `${config.identifier}.installer`, '--version', config.version,
    '--install-location', '/Applications', '--ownership', 'recommended', component,
  ]);
  const cliComponent = join(work, 'locals-studio-cli.pkg');
  const pathComponent = join(work, 'locals-studio-path.pkg');
  for (const [payload, identifier, location, output] of [
    [cliPayload, `${config.identifier}.cli`, '/usr/local/bin', cliComponent],
    [pathPayload, `${config.identifier}.path`, '/etc/paths.d', pathComponent],
  ]) run('/usr/bin/pkgbuild', ['--root', payload, '--identifier', identifier, '--version', config.version, '--install-location', location, '--ownership', 'recommended', output]);

  const requirements = join(work, 'requirements.plist');
  writeFileSync(requirements, `<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>arch</key><array><string>arm64</string></array>
<key>os</key><array><string>${xml(config.bundle.macOS.minimumSystemVersion)}</string></array>
<key>home</key><false/>
</dict></plist>\n`);
  const distribution = join(work, 'Distribution.xml');
  run('/usr/bin/productbuild', ['--synthesize', '--product', requirements, '--package', component, '--package', cliComponent, '--package', pathComponent, distribution]);
  let source = readFileSync(distribution, 'utf8');
  source = distributionElement(source, 'title', `<title>${xml(config.productName)}</title>`);
  source = distributionElement(source, 'domains', '<domains enable_localSystem="true" enable_currentUserHome="false" enable_anywhere="false"/>');
  source = distributionElement(source, 'welcome', '<welcome file="welcome.html"/>');
  source = distributionElement(source, 'conclusion', '<conclusion file="conclusion.html"/>');
  writeFileSync(distribution, source);
  const signing = process.env.LOCALS_STUDIO_INSTALLER_SIGNING_IDENTITY;
  run('/usr/bin/productbuild', [
    '--distribution', distribution, '--package-path', work,
    '--resources', join(root, 'resources/installer'),
    ...(signing ? ['--sign', signing] : []), temporaryOutput,
  ]);
  renameSync(temporaryOutput, output);
  console.log(`Built ${signing ? 'signed' : 'unsigned local'} installer: ${output}`);
} finally {
  rmSync(temporaryOutput, { force: true });
  rmSync(work, { recursive: true, force: true });
}
