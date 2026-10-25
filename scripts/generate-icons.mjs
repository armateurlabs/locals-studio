import { readFileSync, writeFileSync, mkdtempSync, copyFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { inflateSync } from 'node:zlib';

const root = fileURLToPath(new URL('..', import.meta.url));
const brand = JSON.parse(readFileSync(join(root, 'src/brand.json'), 'utf8'));
const bars = brand.bars.map(bar => `<rect x="${bar.x}" y="${bar.y}" width="${bar.width}" height="${bar.height}" rx="${bar.radius}"/>`).join('');
const mark = `<g transform="${brand.markTransform}">${bars}</g>`;
const tile = brand.tile;
const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024"><defs><linearGradient id="background" x1="0" x2="1" y1="0" y2="1"><stop offset="0" stop-color="${brand.backgroundStart}"/><stop offset="1" stop-color="${brand.backgroundEnd}"/></linearGradient><linearGradient id="mark" x1="0" x2=".7" y1="0" y2="1"><stop offset="0" stop-color="${brand.markStart}"/><stop offset="1" stop-color="${brand.markEnd}"/></linearGradient></defs><rect x="${tile.x}" y="${tile.y}" width="${tile.size}" height="${tile.size}" rx="${tile.radius}" fill="url(#background)"/><rect x="${tile.x + 1}" y="${tile.y + 1}" width="${tile.size - 2}" height="${tile.size - 2}" rx="${tile.radius - 1}" fill="none" stroke="${brand.edge}" stroke-opacity=".35" stroke-width="2"/><g fill="url(#mark)">${mark}</g></svg>\n`;
// The menu bar uses the same three bars, enlarged within a transparent canvas.
const tray = `<svg xmlns="http://www.w3.org/2000/svg" width="36" height="36" viewBox="200 135 620 620"><g fill="#000">${mark}</g></svg>\n`;
const icons = join(root, 'src-tauri/icons');
writeFileSync(join(icons, 'icon.svg'), svg);
writeFileSync(join(icons, 'tray.svg'), tray);
const temporary = mkdtempSync(join(tmpdir(), 'locals-studio-icons-'));
function render(args) {
  const result = spawnSync(resolve(root, 'node_modules/.bin/tauri'), ['icon', ...args], { cwd: root, stdio: 'inherit' });
  if (result.status !== 0) throw new Error('Could not generate application icons.');
}
// Decode our generated 8-bit RGBA PNG at build time, keeping image codecs out of the app.
function rgbaFromGeneratedPng(path) {
  const png = readFileSync(path);
  const chunks = [];
  let width, height;
  for (let offset = 8; offset < png.length;) {
    const length = png.readUInt32BE(offset);
    const kind = png.toString('ascii', offset + 4, offset + 8);
    const data = png.subarray(offset + 8, offset + 8 + length);
    if (kind === 'IHDR') {
      width = data.readUInt32BE(0); height = data.readUInt32BE(4);
      if (data[8] !== 8 || data[9] !== 6 || data[12] !== 0) throw new Error('Expected a non-interlaced RGBA icon.');
    }
    if (kind === 'IDAT') chunks.push(data);
    offset += length + 12;
  }
  if (width !== 36 || height !== 36) throw new Error('Expected a 36×36 menu-bar icon.');
  const data = inflateSync(Buffer.concat(chunks));
  const stride = width * 4;
  if (data.length !== height * (stride + 1)) throw new Error('Unexpected PNG data length.');
  const pixels = Buffer.alloc(width * height * 4);
  const paeth = (a, b, c) => { const p = a + b - c, x = Math.abs(p - a), y = Math.abs(p - b), z = Math.abs(p - c); return x <= y && x <= z ? a : y <= z ? b : c; };
  for (let y = 0; y < height; y++) {
    const filter = data[y * (stride + 1)];
    if (filter > 4) throw new Error('Unsupported PNG row filter.');
    for (let x = 0; x < stride; x++) {
      const index = y * stride + x;
      const a = x >= 4 ? pixels[index - 4] : 0;
      const b = y ? pixels[index - stride] : 0;
      const c = y && x >= 4 ? pixels[index - stride - 4] : 0;
      const prediction = [0, a, b, Math.floor((a + b) / 2), paeth(a, b, c)][filter];
      pixels[index] = (data[y * (stride + 1) + x + 1] + prediction) & 255;
    }
  }
  return pixels;
}
try {
  render([join(icons, 'icon.svg'), '--output', join(temporary, 'app')]);
  for (const file of ['32x32.png', '128x128.png', '128x128@2x.png', 'icon.png', 'icon.icns']) copyFileSync(join(temporary, 'app', file), join(icons, file));
  render([join(icons, 'icon.svg'), '--output', join(temporary, 'small'), '--png', '64']);
  copyFileSync(join(temporary, 'small', '64x64.png'), join(icons, '64x64.png'));
  render([join(icons, 'tray.svg'), '--output', join(temporary, 'tray'), '--png', '36']);
  copyFileSync(join(temporary, 'tray', '36x36.png'), join(icons, 'tray.png'));
  writeFileSync(join(icons, 'tray.rgba'), rgbaFromGeneratedPng(join(icons, 'tray.png')));
} finally { rmSync(temporary, { recursive: true, force: true }); }
