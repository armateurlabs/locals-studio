// Write the Sparkle appcast for one release: the DMG GitHub serves for the tag, with its EdDSA
// signature. The app reads it from the latest release's appcast.xml.
import { readFileSync, statSync, writeFileSync } from 'node:fs';
import { basename, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const REPOSITORY = 'armateurlabs/locals-studio';
const escape = value => String(value).replace(/[&<>"']/g, character => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&apos;' })[character]);

/** GitHub stores an uploaded asset under its name with spaces replaced by dots. */
export const assetName = file => basename(file).replaceAll(' ', '.');

export function appcast({ version, tag, archive, length, signature, minimumSystemVersion, date = new Date() }) {
  if (!/^\d+\.\d+\.\d+$/.test(version)) throw new Error(`Invalid version ${version}`);
  if (!/^[A-Za-z0-9+/]{86}==$/.test(signature)) throw new Error('The EdDSA signature is not a base64 Ed25519 signature');
  if (!Number.isInteger(length) || length <= 0) throw new Error('The update archive is empty');
  const url = `https://github.com/${REPOSITORY}/releases/download/${encodeURIComponent(tag)}/${encodeURIComponent(assetName(archive))}`;
  return `<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
  <channel>
    <title>Locals Studio</title>
    <link>https://github.com/${REPOSITORY}/releases</link>
    <item>
      <title>Locals Studio ${escape(version)}</title>
      <pubDate>${date.toUTCString()}</pubDate>
      <sparkle:version>${escape(version)}</sparkle:version>
      <sparkle:shortVersionString>${escape(version)}</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>${escape(minimumSystemVersion)}</sparkle:minimumSystemVersion>
      <sparkle:fullReleaseNotesLink>https://github.com/${REPOSITORY}/releases/tag/${escape(encodeURIComponent(tag))}</sparkle:fullReleaseNotesLink>
      <enclosure url="${escape(url)}" length="${length}" type="application/octet-stream" sparkle:edSignature="${escape(signature)}" />
    </item>
  </channel>
</rss>
`;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const [archive, signature, output] = process.argv.slice(2);
  if (!archive || !signature || !output) throw new Error('Usage: write-appcast.mjs ARCHIVE SIGNATURE OUTPUT');
  const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
  const config = JSON.parse(readFileSync(resolve(root, 'src-tauri/tauri.conf.json'), 'utf8'));
  const tag = process.env.RELEASE_TAG ?? `v${config.version}`;
  if (tag !== `v${config.version}`) throw new Error(`Tag ${tag} does not match the app version ${config.version}`);
  writeFileSync(output, appcast({
    version: config.version, tag, archive, length: statSync(archive).size, signature: signature.trim(),
    minimumSystemVersion: config.bundle.macOS.minimumSystemVersion,
  }));
  console.log(`Wrote ${output}`);
}
