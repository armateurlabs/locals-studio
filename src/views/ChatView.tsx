import { useCallback, useEffect, useMemo, useRef, useState, type DragEvent, type PointerEvent } from 'react';
import { ArrowDown, ArrowUp, ChevronDown, ChevronRight, Folder, FolderPlus, MessageSquare, MoreHorizontal, Pencil, Plus, Settings2, Square, Trash2, X } from 'lucide-react';
import type { Conversation, JsonObject, Snapshot } from '../types';
import { CopyButton, Markdown, Spinner } from '../components';
import { errorMessage } from '../api';
import BrandIcon from '../BrandIcon';

interface Props {
  active: boolean; snapshot: Snapshot; streams: Record<string, string>; errors: Record<string, string>;
  onSend: (conversation: Conversation, text: string, model: string, parameters: JsonObject) => Promise<void>;
  onSave: (conversation: Conversation) => Promise<void>;
  onAction: (method: string, params?: JsonObject) => Promise<any>; onServer: () => void;
}
const freshConversation = (): Conversation => ({ id: crypto.randomUUID(), title: 'New conversation', messages: [], folder_id: null });
const CONVERSATION_DRAG_TYPE = 'application/x-locals-studio-conversation';

export default function ChatView({ active, snapshot, streams, errors, onSend, onSave, onAction, onServer }: Props) {
  const [draft, setDraft] = useState<Conversation>(freshConversation);
  const [selected, setSelected] = useState(draft.id);
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const text = drafts[selected] ?? '';
  const setText = (value: string) => setDrafts(previous => ({ ...previous, [selected]: value }));
  const [model, setModel] = useState('');
  const modelChoice = useRef<'auto' | 'manual' | 'conversation'>('auto');
  const [parametersOpen, setParametersOpen] = useState(false);
  const [temperature, setTemperature] = useState(0.7);
  const [maxTokens, setMaxTokens] = useState(1024);
  const [systemPrompt, setSystemPrompt] = useState('');
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({});
  const [editing, setEditing] = useState(false);
  const [title, setTitle] = useState('');
  const [error, setError] = useState('');
  const [folderName, setFolderName] = useState<string | null>(null);
  const [editingFolder, setEditingFolder] = useState<string | null>(null);
  const [draggedId, setDraggedId] = useState<string | null>(null);
  const [dropTarget, setDropTarget] = useState<string | null>(null);
  const [movingId, setMovingId] = useState<string | null>(null);
  const draggedConversation = useRef<string | null>(null);
  const pointerDrag = useRef<{ id: string; pointerId: number; startX: number; startY: number; dragging: boolean } | null>(null);
  const suppressDragClick = useRef(false);
  const movingConversation = useRef<string | null>(null);
  const [savingTitle, setSavingTitle] = useState<string | null>(null);
  const titleSave = useRef<string | null>(null);
  const end = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLTextAreaElement>(null);
  const scroll = useRef<HTMLDivElement>(null);
  const follow = useRef(true);
  const conversation = snapshot.conversations.find(item => item.id === selected) ?? (draft.id === selected ? draft : freshConversation());
  const streaming = !!streams[selected];
  const profiles = useMemo(() => [...snapshot.profiles].sort((a, b) => a.alias.localeCompare(b.alias, undefined, { numeric: true, sensitivity: 'base' }) || a.id.localeCompare(b.id)), [snapshot.profiles]);
  const availability = (alias: string) => {
    const profile = profiles.find(candidate => candidate.alias === alias);
    const server = snapshot.servers.find(candidate => candidate.alias === alias && candidate.profile_id === profile?.id)
      ?? snapshot.servers.find(candidate => candidate.alias === alias && ['ready', 'running'].includes(candidate.state))
      ?? snapshot.servers.find(candidate => candidate.alias === alias);
    return modelAvailability(server?.state, !!profile);
  };
  const selectedAvailability = model ? availability(model) : 'No model';
  const modelRunning = selectedAvailability === 'Ready';
  const apiRunning = snapshot.api?.running !== false;
  const firstReadyAlias = profiles.find(profile => availability(profile.alias) === 'Ready')?.alias;
  const preferredAlias = firstReadyAlias ?? profiles[0]?.alias ?? '';
  const newChat = useCallback((folderId: string | null = null) => { const next = { ...freshConversation(), folder_id: folderId }; modelChoice.current = 'auto'; setDraft(next); setSelected(next.id); setModel(preferredAlias); setError(''); if (folderId) setCollapsed(previous => ({ ...previous, [folderId]: false })); window.setTimeout(() => input.current?.focus(), 0); }, [preferredAlias]);
  useEffect(() => {
    if (modelChoice.current !== 'auto' || conversation.messages.length || streaming) return;
    const registered = profiles.some(profile => profile.alias === model);
    if (registered && modelRunning) return;
    const next = firstReadyAlias ?? (registered ? model : profiles[0]?.alias ?? '');
    if (next !== model) setModel(next);
  }, [profiles, snapshot.servers, firstReadyAlias, model, modelRunning, conversation.messages.length, streaming]);
  useEffect(() => { if (selected !== draft.id && !snapshot.conversations.some(item => item.id === selected)) newChat(); }, [snapshot.conversations, selected, draft.id, newChat]);
  useEffect(() => { if (active && follow.current) end.current?.scrollIntoView({ behavior: 'auto' }); }, [conversation.messages, selected, active]);
  useEffect(() => { setTitle(conversation.title); setEditing(false); if (conversation.model) { modelChoice.current = 'conversation'; setModel(conversation.model); } follow.current = true; }, [selected]);
  const perform = async (method: string, params: JsonObject) => { setError(''); try { await onAction(method, params); return true; } catch (failure) { setError(errorMessage(failure)); return false; } };
  useEffect(() => { const handler = (event: KeyboardEvent) => { if (active && !document.querySelector('[aria-modal="true"]') && (event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'n') { event.preventDefault(); newChat(); } }; window.addEventListener('keydown', handler); return () => window.removeEventListener('keydown', handler); }, [active, newChat]);
  const submit = async () => {
    if (!text.trim() || !model || !modelRunning || !apiRunning || streaming || movingConversation.current === conversation.id || titleSave.current === conversation.id) return;
    const prompt = text.trim(); setText(''); follow.current = true;
    const messages = systemPrompt && !conversation.messages.length ? [{ role: 'system', content: systemPrompt }] : [];
    await onSend({ ...conversation, messages: [...messages, ...conversation.messages] }, prompt, model, { temperature, max_tokens: maxTokens });
  };
  const conversations = (folderId: string | null) => snapshot.conversations.filter(item => (item.folder_id ?? null) === folderId).sort((a, b) => (b.updated_at ?? '').localeCompare(a.updated_at ?? ''));
  const movingSelected = movingId === conversation.id;
  const savingSelected = savingTitle === conversation.id;
  const saveTitle = async () => {
    if (!title.trim() || movingConversation.current === conversation.id || titleSave.current) return;
    titleSave.current = conversation.id; setSavingTitle(conversation.id); setError('');
    try { await onSave({ ...conversation, title: title.trim() }); setEditing(false); }
    catch (failure) { setError(errorMessage(failure)); }
    finally { titleSave.current = null; setSavingTitle(null); }
  };
  const clearDrag = () => { draggedConversation.current = null; setDraggedId(null); setDropTarget(null); };
  const moveConversation = async (id: string, folderId: string | null) => {
    const source = snapshot.conversations.find(entry => entry.id === id);
    if (!source || (source.folder_id ?? null) === folderId || movingConversation.current || titleSave.current === id || (selected === id && editing) || streams[id]?.startsWith('starting:')) return;
    movingConversation.current = id; setMovingId(id); setError('');
    try {
      await onAction('conversations.move', { id, folder_id: folderId });
      if (folderId) setCollapsed(previous => ({ ...previous, [folderId]: false }));
    } catch (failure) { setError(errorMessage(failure)); }
    finally { movingConversation.current = null; setMovingId(null); }
  };
  const canDrop = (event: DragEvent, folderId: string | null) => {
    const source = snapshot.conversations.find(entry => entry.id === draggedConversation.current);
    return !movingConversation.current && titleSave.current !== source?.id && !(source?.id === selected && editing) && !streams[source?.id ?? '']?.startsWith('starting:') && Array.from(event.dataTransfer.types).includes(CONVERSATION_DRAG_TYPE)
      && !!source && (source.folder_id ?? null) !== folderId;
  };
  const dropHandlers = (folderId: string | null) => ({
    onDragOver: (event: DragEvent<HTMLDivElement>) => {
      if (!canDrop(event, folderId)) return;
      event.preventDefault(); event.stopPropagation(); event.dataTransfer.dropEffect = 'move'; setDropTarget(folderId === null ? 'unfiled' : `folder:${folderId}`);
    },
    onDragLeave: (event: DragEvent<HTMLDivElement>) => {
      if (event.relatedTarget && event.currentTarget.contains(event.relatedTarget as Node)) return;
      setDropTarget(previous => previous === (folderId === null ? 'unfiled' : `folder:${folderId}`) ? null : previous);
    },
    onDrop: (event: DragEvent<HTMLDivElement>) => {
      const id = draggedConversation.current;
      const accepted = canDrop(event, folderId) && event.dataTransfer.getData(CONVERSATION_DRAG_TYPE) === id;
      if (!id) return;
      event.preventDefault(); event.stopPropagation(); clearDrag();
      if (accepted) void moveConversation(id, folderId);
    },
  });
  const pointerDropTarget = (id: string, clientX: number, clientY: number) => {
    const source = snapshot.conversations.find(entry => entry.id === id);
    if (!source || movingConversation.current || titleSave.current === id || (selected === id && editing) || streams[id]?.startsWith('starting:')) return null;
    const target = document.elementFromPoint?.(clientX, clientY)?.closest<HTMLElement>('[data-conversation-drop-target]');
    if (!target) return null;
    const kind = target.dataset.conversationDropTarget;
    const folderId = kind === 'unfiled' ? null : kind === 'folder' ? target.dataset.folderId : undefined;
    if (folderId === undefined || (folderId !== null && !snapshot.folders.some(folder => folder.id === folderId)) || (source.folder_id ?? null) === folderId) return null;
    return { folderId, key: folderId === null ? 'unfiled' : `folder:${folderId}` };
  };
  const pointerHandlers = (id: string) => ({
    onPointerDown: (event: PointerEvent<HTMLButtonElement>) => {
      if (event.button !== 0 || event.isPrimary === false || !['mouse', 'pen'].includes(event.pointerType) || pointerDrag.current || movingConversation.current || titleSave.current === id || (selected === id && editing) || streams[id]?.startsWith('starting:')) return;
      pointerDrag.current = { id, pointerId: event.pointerId, startX: event.clientX, startY: event.clientY, dragging: false };
      event.currentTarget.setPointerCapture?.(event.pointerId);
    },
    onPointerMove: (event: PointerEvent<HTMLButtonElement>) => {
      const gesture = pointerDrag.current;
      if (!gesture || gesture.pointerId !== event.pointerId) return;
      if (!gesture.dragging) {
        if (Math.hypot(event.clientX - gesture.startX, event.clientY - gesture.startY) < 5) return;
        gesture.dragging = true; draggedConversation.current = gesture.id; setDraggedId(gesture.id);
      }
      event.preventDefault();
      setDropTarget(pointerDropTarget(gesture.id, event.clientX, event.clientY)?.key ?? null);
    },
    onPointerUp: (event: PointerEvent<HTMLButtonElement>) => {
      const gesture = pointerDrag.current;
      if (!gesture || gesture.pointerId !== event.pointerId) return;
      const target = gesture.dragging ? pointerDropTarget(gesture.id, event.clientX, event.clientY) : null;
      pointerDrag.current = null;
      if (event.currentTarget.hasPointerCapture?.(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId);
      clearDrag();
      if (!gesture.dragging) return;
      event.preventDefault(); event.stopPropagation(); suppressDragClick.current = true;
      window.setTimeout(() => { suppressDragClick.current = false; }, 0);
      if (target) void moveConversation(gesture.id, target.folderId);
    },
    onPointerCancel: (event: PointerEvent<HTMLButtonElement>) => {
      if (pointerDrag.current?.pointerId !== event.pointerId) return;
      pointerDrag.current = null;
      if (event.currentTarget.hasPointerCapture?.(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId);
      clearDrag();
    },
    onLostPointerCapture: (event: PointerEvent<HTMLButtonElement>) => {
      if (pointerDrag.current?.pointerId !== event.pointerId) return;
      pointerDrag.current = null; clearDrag();
    },
  });
  const item = (entry: Conversation) => <button key={entry.id} className={`conversation-item ${selected === entry.id ? 'selected' : ''} ${draggedId === entry.id ? 'dragging' : ''}`} draggable={!movingId && savingTitle !== entry.id && !(selected === entry.id && editing) && !streams[entry.id]?.startsWith('starting:')} aria-busy={movingId === entry.id || savingTitle === entry.id} {...pointerHandlers(entry.id)} onDragStart={event => { if (pointerDrag.current || movingConversation.current || titleSave.current === entry.id || (selected === entry.id && editing) || streams[entry.id]?.startsWith('starting:')) { event.preventDefault(); return; } draggedConversation.current = entry.id; setDraggedId(entry.id); event.dataTransfer.effectAllowed = 'move'; event.dataTransfer.setData(CONVERSATION_DRAG_TYPE, entry.id); }} onDragEnd={clearDrag} onClick={event => { if (suppressDragClick.current) { event.preventDefault(); event.stopPropagation(); return; } modelChoice.current = 'conversation'; setSelected(entry.id); setModel(entry.model || preferredAlias); setError(''); }}><MessageSquare size={14} /><span>{entry.title || 'Untitled conversation'}</span>{streams[entry.id] && <i className="stream-indicator" />}</button>;
  const blockedReason = movingSelected ? 'Moving this conversation. Wait until the move is complete before sending.' : savingSelected ? 'Saving this conversation. Wait until the save is complete before sending.' : !model ? 'Select a model before sending.'
    : !apiRunning ? 'The inference API is stopped. Start the server in Model server before sending.'
    : selectedAvailability === 'Starting' ? `${model} is starting. Wait until it is ready to send.`
    : selectedAvailability === 'Stopping' ? `${model} is stopping. Select another ready model or open Model server.`
    : selectedAvailability === 'Error' ? `${model} has a server error. Open Model server to inspect its logs.`
    : selectedAvailability === 'Unavailable' ? 'This conversation’s model is no longer available. Select another model above.'
    : !modelRunning ? `${model} is stopped. Start it in Model server or select a ready model above.` : '';
  const showServerAction = !!model && (!apiRunning || ['Stopped', 'Starting', 'Stopping', 'Error'].includes(selectedAvailability));
  const serverActionLabel = !apiRunning || selectedAvailability === 'Stopped' ? 'Start in Model server' : 'View Model server';

  const modelPicker = <div className="model-picker-wrap"><span className={`model-picker-dot ${selectedAvailability.toLowerCase().replaceAll(' ', '-')}`} /><select aria-label="Chat model" className="model-picker" value={model} onChange={event => { modelChoice.current = 'manual'; setModel(event.target.value); }}><option value="">Select a model</option>{model && !profiles.some(profile => profile.alias === model) && <option value={model}>{model} · {selectedAvailability}</option>}{profiles.map(profile => <option key={profile.id} value={profile.alias}>{profile.alias} · {availability(profile.alias)}</option>)}</select><span className={`chat-model-status ${selectedAvailability.toLowerCase().replaceAll(' ', '-')}`} role="status" aria-label="Selected model status">{selectedAvailability}</span></div>;
  const chatActions = <div className="toolbar-actions"><button className={`icon-button ${parametersOpen ? 'selected' : ''}`} title="Generation settings" aria-label="Generation settings" onClick={() => setParametersOpen(!parametersOpen)}><Settings2 size={17} /></button>{conversation.messages.length > 0 && <button className="icon-button danger-hover" title="Delete conversation" aria-label="Delete conversation" disabled={streaming} onClick={async () => { if (await perform('conversations.remove', { id: conversation.id })) newChat(); }}><Trash2 size={16} /></button>}</div>;

  return <div className="chat-layout">
    <aside className="conversation-sidebar"><div className="side-heading"><span>Workspace</span><button className="icon-button" title="New folder" aria-label="New folder" onClick={() => { setFolderName(''); setEditingFolder(null); }}><FolderPlus size={16} /></button></div><button className="new-chat" onClick={() => newChat()}><Plus size={15} /> New conversation <kbd>⌘ N</kbd></button>
      <div className="conversation-list"><div className="section-label">CONVERSATIONS</div><div className={`unfiled-conversations conversation-drop-target ${dropTarget === 'unfiled' ? 'drop-active' : ''}`} role="group" aria-label="Conversations without a folder" data-conversation-drop-target="unfiled" {...dropHandlers(null)}>{conversations(null).map(item)}{!conversations(null).length && <div className="conversation-drop-hint">{snapshot.folders.length ? 'No folder · Drop conversations here' : 'Your conversations stay on this Mac.'}</div>}</div>{snapshot.folders.map(folder => <div className={`conversation-folder conversation-drop-target ${dropTarget === `folder:${folder.id}` ? 'drop-active' : ''}`} key={folder.id} role="group" aria-label={`Conversation folder ${folder.name}`} data-conversation-drop-target="folder" data-folder-id={folder.id} {...dropHandlers(folder.id)}><div className="folder-heading"><button aria-label={`${collapsed[folder.id] ? 'Expand' : 'Collapse'} folder ${folder.name}`} onClick={() => setCollapsed(previous => ({ ...previous, [folder.id]: !previous[folder.id] }))}>{collapsed[folder.id] ? <ChevronRight size={13} /> : <ChevronDown size={13} />}<Folder size={14} /><span>{folder.name}</span><small>{conversations(folder.id).length}</small></button><button className="icon-button" title={`New conversation in ${folder.name}`} aria-label={`New conversation in ${folder.name}`} onClick={() => newChat(folder.id)}><Plus size={13} /></button><button className="icon-button folder-remove" title={`Rename folder ${folder.name}`} aria-label={`Rename folder ${folder.name}`} onClick={() => { setEditingFolder(folder.id); setFolderName(folder.name); }}><Pencil size={11} /></button><button className="icon-button folder-remove" title={`Remove folder ${folder.name}`} aria-label={`Remove folder ${folder.name}`} onClick={() => void perform('folders.remove', { id: folder.id })}><X size={12} /></button></div>{!collapsed[folder.id] && conversations(folder.id).map(item)}{!collapsed[folder.id] && !conversations(folder.id).length && <div className="conversation-drop-hint">Drop conversations here</div>}</div>)}
      {folderName !== null && <form className="folder-form" onSubmit={event => { event.preventDefault(); if (!folderName.trim()) return; void perform('folders.save', { folder: { id: editingFolder ?? crypto.randomUUID(), name: folderName.trim() } }).then(success => { if (success) setFolderName(null); }); }}><input autoFocus aria-label="Folder name" placeholder="Folder name" value={folderName} onChange={event => setFolderName(event.target.value)} onKeyDown={event => { if (event.key === 'Escape') setFolderName(null); }} /><button className="icon-button" type="submit" aria-label="Save folder"><Plus size={15} /></button></form>}</div>
      <div className="sidebar-foot"><span className="tag">LOCAL FIRST</span><span>Private by default.</span></div>
    </aside>
    <section className="chat-main">
      {(conversation.messages.length > 0 || conversation.folder_id) && <div className="conversation-title">{editing ? <form onSubmit={event => { event.preventDefault(); void saveTitle(); }}><input autoFocus value={title} aria-label="Conversation title" onChange={event => setTitle(event.target.value)} onBlur={() => setEditing(false)} /></form> : <button disabled={streaming || movingSelected || savingSelected} onClick={() => { if (movingConversation.current === conversation.id || titleSave.current === conversation.id) return; setTitle(conversation.title); setEditing(true); }}>{conversation.title}</button>}<select value={conversation.folder_id ?? ''} aria-label="Conversation folder" disabled={!!movingId || savingSelected || editing || !!streams[selected]?.startsWith('starting:')} onChange={event => { if (streams[selected]?.startsWith('starting:')) return; const folderId = event.target.value || null; if (conversation.id === draft.id && !snapshot.conversations.some(entry => entry.id === conversation.id)) setDraft(previous => ({ ...previous, folder_id: folderId })); else void moveConversation(conversation.id, folderId); }}><option value="">No folder</option>{snapshot.folders.map(folder => <option value={folder.id} key={folder.id}>{folder.name}</option>)}</select></div>}
      <div className="messages-scroll" ref={scroll} onScroll={() => { const element = scroll.current; if (element) follow.current = element.scrollHeight - element.scrollTop - element.clientHeight < 90; }}>
        {!conversation.messages.length ? <div className="chat-welcome"><div className="welcome-symbol"><BrandIcon /></div><span className="eyebrow">YOUR MODELS. YOUR MACHINE.</span><h1>A little closer to<br /><em>your own intelligence.</em></h1><p>A quiet place to think, write, and build.<br />Every conversation stays right here.</p><div className="prompt-suggestions">{['Explain a tricky concept', 'Help me write something', 'Think through an idea'].map((prompt, index) => <button key={prompt} onClick={() => { setText(prompt); input.current?.focus(); }}><span>{['✦', '↗', '⌘'][index]}</span>{prompt}<ArrowUp size={13} /></button>)}</div>{!profiles.length && <button className="text-link" onClick={onServer}>Configure your first model <ArrowUp size={13} /></button>}</div> : <div className="message-list">{conversation.messages.filter(message => message.role !== 'system').map((message, index) => <article className={`message ${message.role}`} key={message.id ?? index}><div className="message-avatar">{message.role === 'user' ? 'Y' : <span className="mini-mark">l</span>}</div><div className="message-body"><div className="message-heading"><strong>{message.role === 'user' ? 'You' : conversation.model || model || 'Locals'}</strong><CopyButton value={message.content} /></div>{message.reasoning_content && <details className="reasoning"><summary>Thinking</summary><Markdown>{message.reasoning_content}</Markdown></details>}<Markdown>{message.content}</Markdown>{(message.usage || message.cancelled) && <div className="message-usage">{message.usage?.completion_tokens != null && <span>{message.usage.completion_tokens} tokens</span>}{message.usage?.prompt_tokens != null && <span>{message.usage.prompt_tokens} prompt tokens</span>}{message.cancelled && <span>Stopped</span>}</div>}</div></article>)}{streaming && <div className="generating"><Spinner size={13} />{conversation.messages.at(-1)?.role === 'assistant' ? 'Generating' : 'Thinking'}<span>…</span></div>}<div ref={end} /></div>}
      </div>
      {(error || errors[selected]) && <div className="chat-error" role="alert">{error || errors[selected]}</div>}
      <div className="composer-area">{!streaming && blockedReason && <div className="composer-availability" id="composer-availability" role="status"><span>{blockedReason}</span>{showServerAction && <button className="text-link" onClick={onServer}>{serverActionLabel}</button>}</div>}<div className="composer"><textarea ref={input} aria-label="Message" value={text} onChange={event => setText(event.target.value)} placeholder={model ? modelRunning && apiRunning ? `Message ${model}…` : 'Start this model in Model server to chat…' : 'Select a model to start a conversation…'} rows={3} onKeyDown={event => { if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) { event.preventDefault(); void submit(); } }} /><div className="composer-bottom"><div className="composer-model-tools">{modelPicker}{chatActions}</div><div className="composer-send-tools"><span className="keyboard-hint">↵ to send</span>{streaming ? <button className="send-button stop" title="Stop generating" aria-label="Stop generating" disabled={streams[selected]?.startsWith('starting:')} onClick={() => void perform('chat.cancel', { id: streams[selected] })}><Square size={14} fill="currentColor" /></button> : <button className="send-button" title={blockedReason || (!text.trim() ? 'Type a message to send.' : 'Send message')} aria-label="Send message" aria-describedby={blockedReason ? 'composer-availability' : undefined} disabled={!text.trim() || !model || !modelRunning || !apiRunning || movingSelected || savingSelected} onClick={() => void submit()}><ArrowUp size={18} /></button>}</div></div></div></div>
    </section>
    {parametersOpen && <aside className="chat-parameters"><div className="side-heading"><span>Inference</span><button className="icon-button" aria-label="Close inference settings" onClick={() => setParametersOpen(false)}><X size={16} /></button></div><div className="parameters-content"><div className="section-label">GENERATION</div><label className="field">Temperature <span className="field-value">{temperature}</span><input type="range" min="0" max="2" step="0.05" value={temperature} onChange={event => setTemperature(Number(event.target.value))} /><small>Lower for precision, higher for variety.</small></label><label className="field">Maximum output tokens<input type="number" min="1" max="65536" value={maxTokens} onChange={event => setMaxTokens(Number(event.target.value))} /></label><label className="field">System prompt<textarea rows={7} value={systemPrompt} disabled={conversation.messages.length > 0} onChange={event => setSystemPrompt(event.target.value)} placeholder="How should the model behave?" /><small>Applies when starting a new conversation.</small></label><div className="parameter-note"><MessageSquare size={15} /><p>Model loading and memory settings live in <button className="text-link" onClick={onServer}>Model server</button>.</p></div></div></aside>}
  </div>;
}

function modelAvailability(state: string | undefined, registered: boolean) {
  if (state === 'ready' || state === 'running') return 'Ready';
  if (state === 'starting' || state === 'loading') return 'Starting';
  if (state === 'draining' || state === 'stopping') return 'Stopping';
  if (state === 'error' || state === 'failed' || state === 'crashed') return 'Error';
  return registered ? 'Stopped' : 'Unavailable';
}
