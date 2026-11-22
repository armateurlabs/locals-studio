import { useEffect, useState } from 'react';
import { RefreshCw } from 'lucide-react';
import { checkForUpdates, errorMessage, setAutomaticUpdates, updateStatus, type UpdateStatus } from './api';
import { Spinner } from './components';
import { APP_BUILD_LABEL } from './build-info';

/** Settings → Updates: Sparkle's daily automatic check, and a check on demand. */
export default function UpdateSettings() {
  const [status, setStatus] = useState<UpdateStatus | null>(null);
  const [busy, setBusy] = useState('');
  const [error, setError] = useState('');
  useEffect(() => { void updateStatus().then(setStatus).catch(failure => setError(errorMessage(failure))); }, []);
  const act = async (name: string, run: () => Promise<UpdateStatus>) => {
    setBusy(name); setError('');
    try { setStatus(await run()); } catch (failure) { setError(errorMessage(failure)); } finally { setBusy(''); }
  };
  const available = status?.available === true;
  return <>
    <div className="section-label">UPDATES</div>
    <label className="desktop-preference"><input type="checkbox" aria-label="Check for updates automatically" checked={available && status?.automatic === true} disabled={!available || !!busy} onChange={event => { const enabled = event.target.checked; void act('automatic', () => setAutomaticUpdates(enabled)); }} /><span><strong>Check for updates automatically</strong><small>Once a day, Locals Studio looks for a new release on GitHub and offers to install it. Updates are signed and verified before they install.</small></span></label>
    <div className="update-row"><span className="subtle tiny">{!status ? 'Reading update settings…' : !available ? 'Updates are available in the installed app.' : `${APP_BUILD_LABEL} · ${status.last_check ? `last checked ${new Date(status.last_check).toLocaleString()}` : 'not checked yet'}`}</span><button type="button" disabled={!available || !!busy || status?.can_check === false} onClick={() => void act('check', checkForUpdates)}>{busy === 'check' ? <Spinner size={14} /> : <RefreshCw size={14} />} Check for updates</button></div>
    {error && <p className="inline-error" role="alert">{error}</p>}
  </>;
}
