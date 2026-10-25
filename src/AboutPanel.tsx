import { useState } from 'react';
import { ArrowUpRight, Scale } from 'lucide-react';
import { errorMessage, openExternal } from './api';
import { STUDIO_LICENSE } from './license';
import BrandIcon from './BrandIcon';

const ARMATEUR_URL = 'https://armateurlabs.fr';
const COPYRIGHT = STUDIO_LICENSE.split('\n').find(line => line.startsWith('Copyright'));

export default function AboutPanel() {
  const [error, setError] = useState('');
  return <div className="about-panel">
    <div className="about-product"><div className="about-logo" aria-hidden="true"><BrandIcon /></div><h2>Locals Studio</h2><span className="tag">Version 0.1.0</span><p>Your local model workspace.</p></div>
    <div className="about-attribution"><span>By</span><a href={ARMATEUR_URL} aria-label="Visit Armateur Labs" target="_blank" rel="noreferrer" onClick={async event => { event.preventDefault(); setError(''); try { await openExternal(ARMATEUR_URL); } catch (failure) { setError(errorMessage(failure)); } }}>Armateur Labs <ArrowUpRight size={14} /></a><span className="about-url">armateurlabs.fr</span></div>
    {error && <p className="inline-error" role="alert">{error}</p>}
    <section className="about-license" aria-label="Locals Studio license"><div><Scale size={17} /><h3>MIT License</h3></div><p className="about-copyright">{COPYRIGHT}</p><details><summary>View the full MIT License</summary><pre tabIndex={0} aria-label="MIT License text">{STUDIO_LICENSE}</pre></details></section>
    <p className="about-dependencies">Locals is a separate backend under Apache-2.0. Downloaded models have their own licenses.</p>
  </div>;
}
