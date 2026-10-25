import { useId } from 'react';
import brand from './brand.json';

export default function BrandIcon() {
  const id = useId().replaceAll(':', '');
  return <svg className="studio-brand-icon" viewBox={`0 0 ${brand.size} ${brand.size}`} xmlns="http://www.w3.org/2000/svg" aria-hidden="true">
    <defs><linearGradient id={`${id}-background`} x1="0" x2="1" y1="0" y2="1"><stop offset="0" stopColor={brand.backgroundStart} /><stop offset="1" stopColor={brand.backgroundEnd} /></linearGradient><linearGradient id={`${id}-mark`} x1="0" x2=".7" y1="0" y2="1"><stop offset="0" stopColor={brand.markStart} /><stop offset="1" stopColor={brand.markEnd} /></linearGradient></defs>
    <rect x={brand.tile.x} y={brand.tile.y} width={brand.tile.size} height={brand.tile.size} rx={brand.tile.radius} fill={`url(#${id}-background)`} />
    <rect x={brand.tile.x + 1} y={brand.tile.y + 1} width={brand.tile.size - 2} height={brand.tile.size - 2} rx={brand.tile.radius - 1} fill="none" stroke={brand.edge} strokeOpacity=".35" strokeWidth="2" />
    <g fill={`url(#${id}-mark)`} transform={brand.markTransform}>{brand.bars.map(bar => <rect key={bar.x} x={bar.x} y={bar.y} width={bar.width} height={bar.height} rx={bar.radius} />)}</g>
  </svg>;
}
