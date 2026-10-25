import { useEffect, useRef, useState } from 'react';
import { ArrowDown, ArrowUp, ChevronDown, ChevronRight, Folder, FolderPlus, MessageSquare, MoreHorizontal, Pencil, Plus, Settings2, Square, Trash2, X } from 'lucide-react';
import type { Conversation, JsonObject, Snapshot } from '../types';
import { CopyButton, Markdown, Spinner } from '../components';
import { errorMessage } from '../api';

interface Props {
  active: boolean; snapshot: Snapshot; streams: Record<string, string>; errors: Record<string, string>;
  onSend: (conversation: Conversation, text: string, model: string, parameters: JsonObject) => Promise<void>;
  onSave: (conversation: Conversation) => Promise<void>;
  onAction: (method: string, params?: JsonObject) => Promise<any>; onServer: () => void;
}
const freshConversation = (): Conversation => ({ id: crypto.randomUUID(), title: 'New conversation', messages: [], folder_id: null });

export default function ChatView({ active, snapshot, streams, errors, onSend, onSave, onAction, onServer }: Props) {
  const [draft, setDraft] = useState<Conversation>(freshConversation);
  const [selected, setSelected] = useState(draft.id);
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const text = drafts[selected] ?? '';
  const setText = (value: string) => setDrafts(previous => ({ ...previous, [selected]: value }));
  const [model, setModel] = useState('');
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
  const end = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLTextAreaElement>(null);
  const scroll = useRef<HTMLDivElement>(null);
  const follow = useRef(true);
  const conversation = snapshot.conversations.find(item => item.id === selected) ?? (draft.id === selected ? draft : freshConversation());
  const streaming = !!streams[selected];
  const modelRunning = snapshot.servers.some(server => server.alias === model && ['running', 'ready'].includes(server.state));
  const profiles = snapshot.profiles;
  useEffect(() => { if (!model && profiles.length) setModel(profiles[0].alias); else if (model && !profiles.some(profile => profile.alias === model)) setModel(''); }, [profiles, model]);
  useEffect(() => { if (selected !== draft.id && !snapshot.conversations.some(item => item.id === selected)) { const next = freshConversation(); setDraft(next); setSelected(next.id); } }, [snapshot.conversations, selected, draft.id]);
  useEffect(() => { if (active && follow.current) end.current?.scrollIntoView({ behavior: 'auto' }); }, [conversation.messages, selected, active]);
  useEffect(() => { setTitle(conversation.title); setEditing(false); if (conversation.model) setModel(conversation.model); follow.current = true; }, [selected]);
  const newChat = (folderId: string | null = null) => { const next = { ...freshConversation(), folder_id: folderId }; setDraft(next); setSelected(next.id); setError(''); window.setTimeout(() => input.current?.focus(), 0); };
  const perform = async (method: string, params: JsonObject) => { setError(''); try { await onAction(method, params); return true; } catch (failure) { setError(errorMessage(failure)); return false; } };
  useEffect(() => { const handler = (event: KeyboardEvent) => { if (active && !document.querySelector('[aria-modal="true"]') && (event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'n') { event.preventDefault(); newChat(); } }; window.addEventListener('keydown', handler); return () => window.removeEventListener('keydown', handler); }, [active]);
  const submit = async () => {
    if (!text.trim() || !model || !modelRunning || streaming) return;
    const prompt = text.trim(); setText(''); follow.current = true;
    const messages = systemPrompt && !conversation.messages.length ? [{ role: 'system', content: systemPrompt }] : [];
    await onSend({ ...conversation, messages: [...messages, ...conversation.messages] }, prompt, model, { temperature, max_tokens: maxTokens });
  };
  const conversations = (folderId: string | null) => snapshot.conversations.filter(item => (item.folder_id ?? null) === folderId).sort((a, b) => (b.updated_at ?? '').localeCompare(a.updated_at ?? ''));
  const item = (entry: Conversation) => <button key={entry.id} className={`conversation-item ${selected === entry.id ? 'selected' : ''}`} onClick={() => { setSelected(entry.id); setError(''); }}><MessageSquare size={14} /><span>{entry.title || 'Untitled conversation'}</span>{streams[entry.id] && <i className="stream-indicator" />}</button>;

  return <div className="chat-layout">
    <aside className="conversation-sidebar"><div className="side-heading"><span>Workspace</span><button className="icon-button" title="New folder" aria-label="New folder" onClick={() => { setFolderName(''); setEditingFolder(null); }}><FolderPlus size={16} /></button></div><button className="new-chat" onClick={() => newChat()}><Plus size={15} /> New conversation <kbd>⌘ N</kbd></button>
      <div className="conversation-list"><div className="section-label">CONVERSATIONS</div>{!snapshot.conversations.length && <div className="sidebar-hint">Your conversations stay on this Mac.</div>}{conversations(null).map(item)}{snapshot.folders.map(folder => <div className="conversation-folder" key={folder.id}><div className="folder-heading"><button onClick={() => setCollapsed(previous => ({ ...previous, [folder.id]: !previous[folder.id] }))}>{collapsed[folder.id] ? <ChevronRight size={13} /> : <ChevronDown size={13} />}<Folder size={14} /><span>{folder.name}</span><small>{conversations(folder.id).length}</small></button><button className="icon-button" title={`New conversation in ${folder.name}`} aria-label={`New conversation in ${folder.name}`} onClick={() => newChat(folder.id)}><Plus size={13} /></button><button className="icon-button folder-remove" title={`Rename folder ${folder.name}`} aria-label={`Rename folder ${folder.name}`} onClick={() => { setEditingFolder(folder.id); setFolderName(folder.name); }}><Pencil size={11} /></button><button className="icon-button folder-remove" title={`Remove folder ${folder.name}`} aria-label={`Remove folder ${folder.name}`} onClick={() => void perform('folders.remove', { id: folder.id })}><X size={12} /></button></div>{!collapsed[folder.id] && conversations(folder.id).map(item)}</div>)}
      {folderName !== null && <form className="folder-form" onSubmit={event => { event.preventDefault(); if (!folderName.trim()) return; void perform('folders.save', { folder: { id: editingFolder ?? crypto.randomUUID(), name: folderName.trim() } }).then(success => { if (success) setFolderName(null); }); }}><input autoFocus aria-label="Folder name" placeholder="Folder name" value={folderName} onChange={event => setFolderName(event.target.value)} onKeyDown={event => { if (event.key === 'Escape') setFolderName(null); }} /><button className="icon-button" type="submit" aria-label="Save folder"><Plus size={15} /></button></form>}</div>
      <div className="sidebar-foot"><span className="tag">LOCAL FIRST</span><span>Private by default.</span></div>
    </aside>
    <section className="chat-main"><div className="chat-toolbar"><div className="model-picker-wrap"><span className="model-picker-dot" /><select aria-label="Chat model" className="model-picker" value={model} onChange={event => setModel(event.target.value)}><option value="">Select a model</option>{profiles.map(profile => <option key={profile.id} value={profile.alias}>{profile.alias}</option>)}</select></div><div className="toolbar-actions">{model && !modelRunning && <button className="text-link" onClick={onServer}>Start in Model server</button>}<button className={`icon-button ${parametersOpen ? 'selected' : ''}`} title="Generation settings" aria-label="Generation settings" onClick={() => setParametersOpen(!parametersOpen)}><Settings2 size={17} /></button>{conversation.messages.length > 0 && <button className="icon-button danger-hover" title="Delete conversation" aria-label="Delete conversation" disabled={streaming} onClick={async () => { if (await perform('conversations.remove', { id: conversation.id })) newChat(); }}><Trash2 size={16} /></button>}</div></div>
      {conversation.messages.length > 0 && <div className="conversation-title">{editing ? <form onSubmit={event => { event.preventDefault(); if (title.trim()) void onSave({ ...conversation, title: title.trim() }).then(() => setEditing(false)); }}><input autoFocus value={title} aria-label="Conversation title" onChange={event => setTitle(event.target.value)} onBlur={() => setEditing(false)} /></form> : <button disabled={streaming} onClick={() => { setTitle(conversation.title); setEditing(true); }}>{conversation.title}</button>}<select value={conversation.folder_id ?? ''} aria-label="Conversation folder" disabled={streaming} onChange={event => void onSave({ ...conversation, folder_id: event.target.value || null })}><option value="">No folder</option>{snapshot.folders.map(folder => <option value={folder.id} key={folder.id}>{folder.name}</option>)}</select></div>}
      <div className="messages-scroll" ref={scroll} onScroll={() => { const element = scroll.current; if (element) follow.current = element.scrollHeight - element.scrollTop - element.clientHeight < 90; }}>
        {!conversation.messages.length ? <div className="chat-welcome"><div className="welcome-symbol"><span /><span /><span /></div><span className="eyebrow">YOUR MODELS. YOUR MACHINE.</span><h1>A little closer to<br /><em>your own intelligence.</em></h1><p>A quiet place to think, write, and build.<br />Every conversation stays right here.</p><div className="prompt-suggestions">{['Explain a tricky concept', 'Help me write something', 'Think through an idea'].map((prompt, index) => <button key={prompt} onClick={() => { setText(prompt); input.current?.focus(); }}><span>{['✦', '↗', '⌘'][index]}</span>{prompt}<ArrowUp size={13} /></button>)}</div>{!profiles.length && <button className="text-link" onClick={onServer}>Configure your first model <ArrowUp size={13} /></button>}</div> : <div className="message-list">{conversation.messages.filter(message => message.role !== 'system').map((message, index) => <article className={`message ${message.role}`} key={message.id ?? index}><div className="message-avatar">{message.role === 'user' ? 'Y' : <span className="mini-mark">l</span>}</div><div className="message-body"><div className="message-heading"><strong>{message.role === 'user' ? 'You' : conversation.model || model || 'Locals'}</strong><CopyButton value={message.content} /></div>{message.reasoning_content && <details className="reasoning"><summary>Thinking</summary><Markdown>{message.reasoning_content}</Markdown></details>}<Markdown>{message.content}</Markdown>{(message.usage || message.cancelled) && <div className="message-usage">{message.usage?.completion_tokens != null && <span>{message.usage.completion_tokens} tokens</span>}{message.usage?.prompt_tokens != null && <span>{message.usage.prompt_tokens} prompt tokens</span>}{message.cancelled && <span>Stopped</span>}</div>}</div></article>)}{streaming && <div className="generating"><Spinner size={13} />{conversation.messages.at(-1)?.role === 'assistant' ? 'Generating' : 'Thinking'}<span>…</span></div>}<div ref={end} /></div>}
      </div>
      {(error || errors[selected]) && <div className="chat-error" role="alert">{error || errors[selected]}</div>}
      <div className="composer-area"><div className="composer"><textarea ref={input} aria-label="Message" value={text} onChange={event => setText(event.target.value)} placeholder={model ? modelRunning ? `Message ${model}…` : 'Start this model in Model server to chat…' : 'Select a model to start a conversation…'} rows={3} onKeyDown={event => { if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) { event.preventDefault(); void submit(); } }} /><div className="composer-bottom"><span><span className="composer-local-dot" /> Local & private</span><div><span className="keyboard-hint">↵ to send</span>{streaming ? <button className="send-button stop" title="Stop generating" aria-label="Stop generating" disabled={streams[selected]?.startsWith('starting:')} onClick={() => void perform('chat.cancel', { id: streams[selected] })}><Square size={14} fill="currentColor" /></button> : <button className="send-button" title="Send message" aria-label="Send message" disabled={!text.trim() || !model || !modelRunning} onClick={() => void submit()}><ArrowUp size={18} /></button>}</div></div></div></div>
    </section>
    {parametersOpen && <aside className="chat-parameters"><div className="side-heading"><span>Inference</span><button className="icon-button" aria-label="Close inference settings" onClick={() => setParametersOpen(false)}><X size={16} /></button></div><div className="parameters-content"><div className="section-label">GENERATION</div><label className="field">Temperature <span className="field-value">{temperature}</span><input type="range" min="0" max="2" step="0.05" value={temperature} onChange={event => setTemperature(Number(event.target.value))} /><small>Lower for precision, higher for variety.</small></label><label className="field">Maximum output tokens<input type="number" min="1" max="65536" value={maxTokens} onChange={event => setMaxTokens(Number(event.target.value))} /></label><label className="field">System prompt<textarea rows={7} value={systemPrompt} disabled={conversation.messages.length > 0} onChange={event => setSystemPrompt(event.target.value)} placeholder="How should the model behave?" /><small>Applies when starting a new conversation.</small></label><div className="parameter-note"><MessageSquare size={15} /><p>Model loading and memory settings live in <button className="text-link" onClick={onServer}>Model server</button>.</p></div></div></aside>}
  </div>;
}
