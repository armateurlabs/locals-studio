import { Check } from 'lucide-react';

export type ColorMode = 'dark' | 'light';
export type Palette = 'green' | 'sapphire' | 'violet' | 'amber' | 'rose';
export type TextSize = 'standard' | 'large';
export interface Appearance { theme: ColorMode; palette: Palette; textSize: TextSize; }

export const DEFAULT_APPEARANCE: Appearance = { theme: 'dark', palette: 'green', textSize: 'standard' };
export const PALETTES: { value: Palette; label: string; swatch: string }[] = [
  { value: 'green', label: 'Green', swatch: '#91cda6' },
  { value: 'sapphire', label: 'Sapphire', swatch: '#81afea' },
  { value: 'violet', label: 'Violet', swatch: '#b79bdc' },
  { value: 'amber', label: 'Amber', swatch: '#dcb36d' },
  { value: 'rose', label: 'Rose', swatch: '#dfa4bc' },
];

export function normalizeAppearance(value: { theme?: unknown; palette?: unknown; textSize?: unknown } = {}): Appearance {
  return {
    theme: value.theme === 'light' || value.theme === 'dark' ? value.theme : DEFAULT_APPEARANCE.theme,
    palette: PALETTES.some(palette => palette.value === value.palette) ? value.palette as Palette : DEFAULT_APPEARANCE.palette,
    textSize: value.textSize === 'standard' || value.textSize === 'large' ? value.textSize : DEFAULT_APPEARANCE.textSize,
  };
}

export function loadAppearance(): Appearance {
  try { return normalizeAppearance({ theme: localStorage.getItem('studio.theme'), palette: localStorage.getItem('studio.palette'), textSize: localStorage.getItem('studio.textSize') }); }
  catch { return { ...DEFAULT_APPEARANCE }; }
}

export function applyAppearance(value: Appearance) {
  const appearance = normalizeAppearance(value);
  document.documentElement.dataset.theme = appearance.theme;
  document.documentElement.dataset.palette = appearance.palette;
  document.documentElement.dataset.textSize = appearance.textSize;
  try {
    localStorage.setItem('studio.theme', appearance.theme);
    localStorage.setItem('studio.palette', appearance.palette);
    localStorage.setItem('studio.textSize', appearance.textSize);
  } catch { /* Visual preferences still apply when local storage is unavailable. */ }
}

export function SettingsAppearance({ value, onChange }: { value: Appearance; onChange: (patch: Partial<Appearance>) => void }) {
  return <section className="settings-appearance" aria-label="Appearance">
    <div className="section-label">APPEARANCE</div>
    <div className="appearance-options"><label className="field">Color mode<select aria-label="Color mode" value={value.theme} onChange={event => onChange({ theme: event.target.value as ColorMode })}><option value="dark">Dark</option><option value="light">Light</option></select></label><label className="field">Text size<select aria-label="Text size" value={value.textSize} onChange={event => onChange({ textSize: event.target.value as TextSize })}><option value="standard">Standard</option><option value="large">Large</option></select></label></div>
    <fieldset className="appearance-palettes"><legend>Color palette</legend><div role="group" aria-label="Color palette">{PALETTES.map(palette => <button type="button" key={palette.value} aria-label={`${palette.label} palette`} aria-pressed={value.palette === palette.value} onClick={() => onChange({ palette: palette.value })}><span className="appearance-swatch" aria-hidden="true" style={{ backgroundColor: palette.swatch }} /><span>{palette.label}</span>{value.palette === palette.value && <Check size={11} aria-hidden="true" />}</button>)}</div></fieldset>
    <p className="appearance-hint">Changes apply immediately and are saved on this Mac.</p>
  </section>;
}
