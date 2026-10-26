import { Check, Copy, LoaderCircle, X } from 'lucide-react';
import { useEffect, useRef, useState, type ReactNode } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';

export function Spinner({ size = 16 }: { size?: number }) { return <LoaderCircle size={size} className="spin" aria-label="Working" />; }
export function Status({ state }: { state: string }) {
  const kind = /ready|running|installed|completed|available/.test(state) ? 'good' : /error|failed|crashed/.test(state) ? 'bad' : /loading|starting|downloading|installing/.test(state) ? 'busy' : 'neutral';
  return <span className={`status ${kind}`}><i />{state.replaceAll('_', ' ')}</span>;
}
export function Empty({ icon, title, children, action }: { icon: ReactNode; title: string; children: ReactNode; action?: ReactNode }) {
  return <div className="empty"><div className="empty-icon">{icon}</div><h2>{title}</h2><p>{children}</p>{action}</div>;
}
export function Modal({ title, children, onClose, wide = false }: { title: string; children: ReactNode; onClose: () => void; wide?: boolean }) {
  const dialog = useRef<HTMLElement>(null);
  useEffect(() => { const previous = document.activeElement as HTMLElement | null; if (!dialog.current?.contains(document.activeElement)) dialog.current?.querySelector<HTMLElement>('button,input,textarea,select')?.focus(); return () => previous?.focus(); }, []);
  return <div className="modal-backdrop" onMouseDown={event => { if (event.target === event.currentTarget) onClose(); }}><section ref={dialog} className={`modal ${wide ? 'wide' : ''}`} role="dialog" aria-modal="true" aria-label={title} onKeyDown={event => {
    if (event.key === 'Escape') { event.stopPropagation(); onClose(); }
    if (event.key === 'Tab') {
      const items = [...(dialog.current?.querySelectorAll<HTMLElement>('button:not(:disabled),input:not(:disabled),textarea:not(:disabled),select:not(:disabled),a[href]') ?? [])].filter(item => item.getClientRects().length);
      const first = items[0], last = items.at(-1);
      if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
      else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
    }
  }}><header><h2>{title}</h2><button className="icon-button" aria-label="Close dialog" onClick={onClose}><X size={18} /></button></header>{children}</section></div>;
}
export function CopyButton({ value, disabled = false, label = 'Copy' }: { value: string; disabled?: boolean; label?: string }) {
  const [copied, setCopied] = useState(false);
  return <button className="icon-button" title={label} aria-label={label} disabled={disabled} onClick={async () => { if (disabled) return; await navigator.clipboard.writeText(value); setCopied(true); window.setTimeout(() => setCopied(false), 1800); }}>{copied ? <Check size={14} /> : <Copy size={14} />}</button>;
}
export function Markdown({ children }: { children: string }) {
  return <div className="markdown"><ReactMarkdown remarkPlugins={[remarkGfm]} components={{ pre: ({ children }) => <div className="code-block"><pre>{children}</pre></div>, a: ({ children, href }) => <a href={href} target="_blank" rel="noreferrer">{children}</a> }}>{children}</ReactMarkdown></div>;
}
