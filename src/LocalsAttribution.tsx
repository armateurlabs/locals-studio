import { useState } from 'react';
import { ArrowUpRight } from 'lucide-react';
import { errorMessage, openExternal } from './api';

const GENERATED_COMPUTERS_URL = 'https://generatedcomputers.com/';

export default function LocalsAttribution() {
  const [error, setError] = useState('');
  return <section className="locals-attribution" aria-label="Locals attribution and independence">
    <p>Locals Studio is an independent frontend for <strong>Locals</strong>, the inference backend by <a href={GENERATED_COMPUTERS_URL} aria-label="Visit Generated Computers" target="_blank" rel="noreferrer" onClick={async event => {
      event.preventDefault();
      setError('');
      try { await openExternal(GENERATED_COMPUTERS_URL); }
      catch (failure) { setError(errorMessage(failure)); }
    }}>Generated Computers <ArrowUpRight size={13} aria-hidden="true" /></a>.</p>
    <p className="locals-independence">Developed by Armateur Labs. Not affiliated with, endorsed by, or maintained by Generated Computers or the Locals project.</p>
    {error && <p className="inline-error" role="alert">{error}</p>}
  </section>;
}
