import { useId } from 'react';

/** Kapitsa's mark: four bars crossed by a dark stroke that fades in from the left, ending in a K. */
export default function KapitsaLogo({ size = 24, className }: { size?: number; className?: string }) {
  const fade = `kapitsa-fade-${useId().replace(/:/g, '')}`;
  return <svg className={className} width={size} height={size} viewBox="0 0 64 64" aria-hidden="true" focusable="false">
    <defs>
      <linearGradient id={fade} gradientUnits="userSpaceOnUse" x1="2" x2="22" y1="32" y2="32">
        <stop offset="0" stopColor="#2f5d37" stopOpacity="0" />
        <stop offset="1" stopColor="#2f5d37" />
      </linearGradient>
    </defs>
    <g strokeLinecap="round" strokeWidth="4.2" fill="none">
      <line x1="22" y1="13" x2="22" y2="51" stroke="#62b363" />
      <line x1="28" y1="13" x2="28" y2="51" stroke="#c1e8c8" />
      <line x1="34" y1="13" x2="34" y2="51" stroke="#62b363" />
      <line x1="40" y1="13" x2="40" y2="51" stroke="#c1e8c8" />
      <line x1="40" y1="32" x2="54" y2="13" stroke="#62b363" />
      <line x1="40" y1="32" x2="54" y2="51" stroke="#62b363" />
      <line x1="2" y1="32" x2="22" y2="32" stroke={`url(#${fade})`} strokeLinecap="butt" />
      <line x1="22" y1="32" x2="54" y2="32" stroke="#2f5d37" />
    </g>
  </svg>;
}
