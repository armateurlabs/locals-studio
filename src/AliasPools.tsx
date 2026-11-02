import { useEffect, useRef, useState } from 'react';
import { Layers, Pencil, Plus, Trash2 } from 'lucide-react';
import type { JsonObject, ModelAlias, Snapshot } from './types';
import { Modal, Spinner } from './components';
import { errorMessage } from './api';

interface Props {
  snapshot: Snapshot;
  onAction: (method: string, params?: JsonObject) => Promise<any>;
}

export default function AliasPools({ snapshot, onAction }: Props) {
  const aliases = [...(snapshot.aliases ?? [])].sort((a, b) => a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: 'base' }) || a.id.localeCompare(b.id));
  const profiles = [...snapshot.profiles].sort((a, b) => a.alias.localeCompare(b.alias, undefined, { numeric: true, sensitivity: 'base' }) || a.id.localeCompare(b.id));
  const available = Array.isArray(snapshot.service?.capabilities) && snapshot.service.capabilities.includes('model_aliases_v1') && snapshot.service?.upgrade?.status !== 'pending';
  const availableRef = useRef(available);
  availableRef.current = available;
  const [editor, setEditor] = useState<ModelAlias | null>(null);
  const [creating, setCreating] = useState(false);
  const [removeTarget, setRemoveTarget] = useState<ModelAlias | null>(null);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const pending = useRef(false);
  const mounted = useRef(false);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  const missingMembers = editor?.profile_ids.filter(id => !profiles.some(profile => profile.id === id)) ?? [];
  const ready = (id: string) => {
    const profile = profiles.find(profile => profile.id === id);
    return !!profile && snapshot.servers.some(server => server.profile_id === id && server.model_id === profile.model_id && ['ready', 'running'].includes(server.state));
  };
  const openEditor = (alias?: ModelAlias) => {
    if (!availableRef.current || pending.current) return;
    setError(''); setRemoveTarget(null);
    setCreating(!alias);
    setEditor(alias ? { ...alias, profile_ids: [...alias.profile_ids] } : { id: crypto.randomUUID(), name: '', profile_ids: [] });
  };
  const close = () => { if (!pending.current) { setEditor(null); setRemoveTarget(null); setError(''); } };
  const setMember = (id: string, checked: boolean) => {
    if (pending.current) return;
    setEditor(previous => previous ? { ...previous, profile_ids: checked ? [...new Set([...previous.profile_ids, id])] : previous.profile_ids.filter(member => member !== id) } : previous);
    setError('');
  };
  const save = async () => {
    if (!editor || !availableRef.current || pending.current) return;
    const alias = { ...editor, name: editor.name.trim(), profile_ids: [...new Set(editor.profile_ids)] };
    if (!/^[A-Za-z0-9._-]+$/.test(alias.name)) { setError('Use an alias name containing letters, digits, dots, hyphens or underscores.'); return; }
    if (!alias.profile_ids.length) { setError('Choose at least one server for this alias.'); return; }
    if (missingMembers.length) { setError('Remove unavailable servers before saving this alias.'); return; }
    pending.current = true; setBusy(true); setError('');
    try {
      await onAction('aliases.save', { alias });
      if (mounted.current) setEditor(null);
    } catch (failure) { if (mounted.current) setError(errorMessage(failure)); }
    finally { pending.current = false; if (mounted.current) setBusy(false); }
  };
  const remove = async () => {
    if (!removeTarget || !availableRef.current || pending.current) return;
    const id = removeTarget.id;
    pending.current = true; setBusy(true); setError('');
    try {
      await onAction('aliases.remove', { id });
      if (mounted.current) setRemoveTarget(null);
    } catch (failure) { if (mounted.current) setError(errorMessage(failure)); }
    finally { pending.current = false; if (mounted.current) setBusy(false); }
  };

  return <section className="alias-pools" aria-label="Model aliases">
    <div className="alias-pools-heading">
      <div><h2><Layers size={15} />Model aliases</h2><p>One name for several servers. Conversations keep using the same available server, with automatic fallback.</p></div>
      <button disabled={!available || busy} onClick={() => openEditor()}><Plus size={14} />Create alias</button>
    </div>
    {!available && <p className="alias-pools-pending" role="status">Model aliases need the updated Studio service. Studio will retry its background-service update when it is idle.</p>}
    {aliases.length ? <div className="alias-pool-list">{aliases.map(alias => {
      const ids = [...new Set(alias.profile_ids)];
      return <div className="alias-pool-row" key={alias.id} role="group" aria-label={`Model alias ${alias.name}`}>
        <div className="alias-pool-summary"><strong>{alias.name}</strong><span>{ids.filter(ready).length} ready / {ids.length} {ids.length === 1 ? 'server' : 'servers'}</span><small>{ids.map(id => {
          const profile = profiles.find(profile => profile.id === id);
          const model = snapshot.models.find(model => model.id === profile?.model_id);
          return profile ? `${profile.alias}${model ? ` (${model.name})` : ''}` : `Unavailable server (${id})`;
        }).join(' · ')}</small></div>
        <div className="alias-pool-actions"><button aria-label={`Edit alias ${alias.name}`} disabled={!available || busy} onClick={() => openEditor(alias)}><Pencil size={13} />Edit members</button><button className="icon-button danger-hover" aria-label={`Remove alias ${alias.name}`} title="Remove alias" disabled={!available || busy} onClick={() => { if (pending.current || !availableRef.current) return; setError(''); setEditor(null); setRemoveTarget(alias); }}><Trash2 size={14} /></button></div>
      </div>;
    })}</div> : <p className="alias-pools-empty">Create an alias to let clients use one model name across several server profiles.</p>}
    {editor && <Modal title={creating ? 'Create model alias' : 'Edit model alias'} onClose={close}>
      <form onSubmit={event => { event.preventDefault(); void save(); }}>
        <div className="settings-body">
          <label className="field">Alias name<input aria-label="Alias name" required maxLength={128} readOnly={!creating} disabled={busy} value={editor.name} onChange={event => { if (!pending.current && creating) { setEditor({ ...editor, name: event.target.value }); setError(''); } }} placeholder="e.g. assistant" /><small>{creating ? 'Clients use this name in their model field.' : 'The name stays fixed so existing clients and conversations keep working.'}</small></label>
          <div className="section-label">MEMBER SERVERS</div>
          <div className="alias-pool-members">{profiles.map(profile => {
            const model = snapshot.models.find(model => model.id === profile.model_id);
            const server = snapshot.servers.find(server => server.profile_id === profile.id);
            return <label className="alias-pool-member" key={profile.id}><input type="checkbox" aria-label={`Include server ${profile.alias}`} checked={editor.profile_ids.includes(profile.id)} disabled={busy} onChange={event => setMember(profile.id, event.target.checked)} /><span><strong>{profile.alias}</strong><small>{model?.name ?? profile.model_id}</small></span><span className="alias-pool-member-state">{server?.state ?? 'stopped'}</span></label>;
          })}{missingMembers.map(id => <label className="alias-pool-member unavailable" key={id}><input type="checkbox" aria-label={`Include unavailable server ${id}`} checked disabled={busy} onChange={event => setMember(id, event.target.checked)} /><span><strong>Unavailable server</strong><small>{id}</small></span></label>)}</div>
          {!profiles.length && <p className="subtle">Create a server profile from My models before adding it to an alias.</p>}
          {missingMembers.length > 0 && <p className="alias-pools-pending">Remove unavailable servers before saving. Their profiles no longer exist.</p>}
          <p className="subtle alias-pool-explanation">Only ready servers receive requests. Editing this pool does not start or stop its servers.</p>
          {!available && <p className="alias-pools-pending" role="status">Wait for the Studio service update before saving changes.</p>}
          {error && <p className="inline-error" role="alert">{error}</p>}
        </div>
        <footer className="modal-footer"><button type="button" disabled={busy} onClick={close}>Cancel</button><button type="submit" className="primary" disabled={!available || busy || !editor.name.trim() || !editor.profile_ids.length || !!missingMembers.length}>{busy && <Spinner size={14} />}Save alias</button></footer>
      </form>
    </Modal>}
    {removeTarget && <Modal title="Remove model alias?" onClose={close}>
      <div className="settings-body"><p>Remove <strong>{removeTarget.name}</strong>?</p><p className="subtle">Its server profiles and models remain available. Clients and conversations using this alias will need another model name.</p>{error && <p className="inline-error" role="alert">{error}</p>}</div>
      <footer className="modal-footer"><button disabled={busy} onClick={close}>Cancel</button><button className="danger-button" disabled={!available || busy} onClick={() => void remove()}>{busy && <Spinner size={14} />}Remove alias</button></footer>
    </Modal>}
  </section>;
}
