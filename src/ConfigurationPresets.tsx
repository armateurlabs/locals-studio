import { useEffect, useRef, useState } from 'react';
import type { ConfigurationPreset, Engine, JsonObject, Snapshot } from './types';
import { Modal, Spinner } from './components';
import { errorMessage } from './api';

interface Props {
  snapshot: Snapshot;
  /** Presets hold one engine's options; only that engine's are offered. */
  engine?: Engine;
  disabled: boolean;
  getOptions: () => JsonObject;
  onApply: (options: JsonObject) => void;
  onAction: (method: string, params?: JsonObject) => Promise<any>;
}
const available = (snapshot: Snapshot) => Array.isArray(snapshot.service?.capabilities) && snapshot.service.capabilities.includes('configuration_presets_v1') && snapshot.service?.upgrade?.status !== 'pending';
const readOnly = (preset: ConfigurationPreset) => preset.read_only || preset.id.startsWith('builtin:');

export default function ConfigurationPresets({ snapshot, engine = 'locals', disabled, getOptions, onApply, onAction }: Props) {
  const presets = (snapshot.configuration_presets ?? []).filter(preset => (preset.engine ?? 'locals') === engine);
  const [selectedId, setSelectedId] = useState('');
  const [editor, setEditor] = useState<{ id?: string; name: string; options: JsonObject; update?: boolean } | null>(null);
  const [remove, setRemove] = useState<ConfigurationPreset | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const pending = useRef(false);
  const mounted = useRef(false);
  const latest = useRef({ snapshot, disabled, editor, selectedId, remove });
  latest.current = { snapshot, disabled, editor, selectedId, remove };
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  const supported = available(snapshot);
  const blocked = disabled || busy || !supported;
  const selected = presets.find(preset => preset.id === selectedId);
  const allowed = () => mounted.current && !pending.current && !latest.current.disabled && available(latest.current.snapshot);
  const currentPreset = () => latest.current.snapshot.configuration_presets?.find(preset => preset.id === latest.current.selectedId);
  const close = () => { if (!pending.current) { setEditor(null); setRemove(null); setError(''); } };
  const apply = () => {
    if (!allowed()) return;
    const preset = currentPreset();
    if (preset) { onApply(structuredClone(preset.options)); setError(''); }
  };
  const edit = (rename: boolean, update = false) => {
    if (!allowed()) return;
    setError('');
    try {
      const preset = currentPreset();
      if ((rename || update) && (!preset || readOnly(preset))) return;
      setEditor((rename || update) && preset ? { id: preset.id, name: preset.name, options: structuredClone(update ? getOptions() : preset.options), update } : { name: '', options: structuredClone(getOptions()) });
    } catch (failure) { setError(errorMessage(failure)); }
  };
  const save = async () => {
    const draft = latest.current.editor;
    if (!allowed() || !draft || !draft.name.trim()) return;
    pending.current = true; setBusy(true); setError('');
    try {
      const saved = await onAction('configuration_presets.save', { preset: { ...(draft.id ? { id: draft.id } : {}), name: draft.name.trim(), ...(engine !== 'locals' ? { engine } : {}), options: draft.options } });
      if (mounted.current) { setEditor(null); if (saved?.id) setSelectedId(saved.id); }
    } catch (failure) { if (mounted.current) setError(errorMessage(failure)); }
    finally { pending.current = false; if (mounted.current) setBusy(false); }
  };
  const deletePreset = async () => {
    const target = latest.current.remove;
    if (!allowed() || !target || readOnly(target)) return;
    const id = target.id;
    pending.current = true; setBusy(true); setError('');
    try {
      await onAction('configuration_presets.remove', { id });
      if (mounted.current) { setRemove(null); setSelectedId(''); }
    } catch (failure) { if (mounted.current) setError(errorMessage(failure)); }
    finally { pending.current = false; if (mounted.current) setBusy(false); }
  };
  return <section className="configuration-presets" aria-label="Shared configuration presets">
    <label className="field">Shared configuration<select aria-label="Configuration preset" value={selectedId} disabled={blocked} onChange={event => { setSelectedId(event.target.value); setError(''); }}><option value="">Select a preset</option>{selectedId && !selected && <option value={selectedId}>Unavailable preset</option>}{presets.map(preset => <option value={preset.id} key={preset.id}>{preset.name}</option>)}</select></label>
    <div className="button-row configuration-preset-actions"><button type="button" disabled={blocked || !selected} onClick={apply}>Apply preset</button><button type="button" disabled={blocked} onClick={() => edit(false)}>Save as preset</button>{selected && !readOnly(selected) && <><button type="button" disabled={blocked} onClick={() => edit(false, true)}>Update preset</button><button type="button" disabled={blocked} onClick={() => edit(true)}>Rename preset</button><button type="button" disabled={blocked} onClick={() => { if (allowed()) { const preset = currentPreset(); if (preset && !readOnly(preset)) { setError(''); setRemove(preset); } } }}>Delete preset</button></>}</div>
    <p className="subtle tiny">Shared across models. Applying replaces this draft's server options. Save the profile to use them on its next start.</p>
    {!supported && <p className="subtle tiny" role="status">Shared presets need the updated Studio service.</p>}
    {error && !editor && !remove && <p className="inline-error" role="alert">{error}</p>}
    {editor && <Modal title={editor.update ? 'Update configuration preset' : editor.id ? 'Rename configuration preset' : 'Save configuration preset'} onClose={close}>
      <div className="settings-body"><label className="field">Preset name<input aria-label="Preset name" autoFocus maxLength={128} value={editor.name} disabled={busy} onChange={event => { if (!pending.current) setEditor({ ...editor, name: event.target.value }); }} onKeyDown={event => { if (event.key === 'Enter') { event.preventDefault(); event.stopPropagation(); void save(); } }} /></label><pre className="configuration-preset-preview">{JSON.stringify(editor.options, null, 2)}</pre>{error && <p className="inline-error" role="alert">{error}</p>}</div>
      <footer className="modal-footer"><button type="button" disabled={busy} onClick={close}>Cancel</button><button type="button" className="primary" disabled={blocked || !editor.name.trim()} onClick={() => void save()}>{busy && <Spinner size={14} />}Save preset</button></footer>
    </Modal>}
    {remove && <Modal title="Delete configuration preset?" onClose={close}><div className="settings-body"><p>Delete <strong>{remove.name}</strong>?</p><p className="subtle">Profiles that previously applied it keep their saved options.</p>{error && <p className="inline-error" role="alert">{error}</p>}</div><footer className="modal-footer"><button type="button" disabled={busy} onClick={close}>Cancel</button><button type="button" className="danger-button" disabled={blocked} onClick={() => void deletePreset()}>{busy && <Spinner size={14} />}Delete preset</button></footer></Modal>}
  </section>;
}
