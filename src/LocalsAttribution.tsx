import { useState } from 'react';
import { ArrowUpRight } from 'lucide-react';
import { errorMessage, openExternal } from './api';

const GENERATED_COMPUTERS_URL = 'https://generatedcomputers.com/';
const KAPITSA_URL = 'https://github.com/nace-ai/kapitsa';

export default function LocalsAttribution() {
  const [error, setError] = useState('');
  const link = (url: string, label: string, text: string) => <a href={url} aria-label={label} target="_blank" rel="noreferrer" onClick={async event => {
    event.preventDefault();
    setError('');
    try { await openExternal(url); }
    catch (failure) { setError(errorMessage(failure)); }
  }}>{text} <ArrowUpRight size={13} aria-hidden="true" /></a>;
  return <section className="locals-attribution" aria-label="Engines attribution and independence">
    <p>Locals Studio is an independent workspace for the inference engines on your Mac: <strong>Locals</strong> by {link(GENERATED_COMPUTERS_URL, 'Visit Generated Computers', 'Generated Computers')} and <strong>Kapitsa</strong> by {link(KAPITSA_URL, 'Visit Kapitsa on GitHub', 'Nace AI')}. Run them side by side behind one local API.</p>
    <p className="locals-independence">Developed by Armateur Labs. Not affiliated with, endorsed by, or maintained by Generated Computers, Nace AI, or the Locals and Kapitsa projects.</p>
    {error && <p className="inline-error" role="alert">{error}</p>}
  </section>;
}
