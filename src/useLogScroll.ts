import { useEffect, useLayoutEffect, useRef, useState } from 'react';

export type LogOrder = 'asc' | 'desc';
type Anchor = { key: string; offset: number };
type Position = { scope: string; height: number; top: number; anchors: Anchor[] };

function boundedTop(view: HTMLElement) {
  // WebKit can report positions outside the scroll range during overscroll.
  return Math.max(0, Math.min(view.scrollTop, Math.max(0, view.scrollHeight - view.clientHeight)));
}

function rows(view: HTMLElement) {
  return typeof view.querySelectorAll === 'function'
    ? Array.from(view.querySelectorAll<HTMLElement>('[data-log-key]')) : [];
}

function position(view: HTMLElement, scope: string): Position {
  const bounds = view.getBoundingClientRect?.();
  const anchors: Anchor[] = [];
  if (bounds) {
    for (const row of rows(view)) {
      const rect = row.getBoundingClientRect();
      if (rect.top >= bounds.bottom) break;
      if (rect.bottom > bounds.top && row.dataset.logKey != null) {
        anchors.push({ key: row.dataset.logKey, offset: rect.top - bounds.top });
        if (anchors.length === 3) break;
      }
    }
  }
  return { scope, height: view.scrollHeight, top: boundedTop(view), anchors };
}

/** Keep the latest edge in view, or anchor the lines being read while a tail updates. */
export function useLogScroll(open: boolean, scope: string, order: LogOrder, entries: unknown[], height: number) {
  const viewport = useRef<HTMLDivElement>(null);
  const [following, setFollowing] = useState(true);
  const follow = useRef(true);
  const previous = useRef<Position | null>(null);
  const programmaticTop = useRef<number | null>(null);
  const reconcile = useRef(() => {});

  const changeFollowing = (value: boolean) => {
    follow.current = value;
    setFollowing(value);
  };
  const move = (view: HTMLElement, top: number) => {
    const target = Math.max(0, Math.min(top, Math.max(0, view.scrollHeight - view.clientHeight)));
    if (Math.abs(view.scrollTop - target) > 0.5) {
      view.scrollTop = target;
      // Scroll events arrive asynchronously, including after the Follow button is paused.
      programmaticTop.current = boundedTop(view);
    }
  };
  reconcile.current = () => {
    const view = viewport.current;
    if (!open || !view) { previous.current = null; programmaticTop.current = null; return; }
    const last = previous.current;
    if (!last || last.scope !== scope) {
      changeFollowing(true);
      programmaticTop.current = null;
    }
    if (follow.current) {
      move(view, order === 'asc' ? view.scrollHeight - view.clientHeight : 0);
    } else if (last) {
      // Line numbers restart when the bounded log tail rotates. Match the visible
      // content instead, including its recorded timestamp, before falling back to geometry.
      const bounds = view.getBoundingClientRect?.();
      const currentRows = rows(view);
      let anchored = false;
      if (bounds) {
        for (const anchor of last.anchors) {
          // Repeated legacy lines have no unique ID. Prefer the occurrence near
          // its expected position instead of jumping to the first matching text.
          const expected = anchor.offset + (order === 'desc' ? view.scrollHeight - last.height : 0);
          const row = currentRows.filter(candidate => candidate.dataset.logKey === anchor.key)
            .reduce<HTMLElement | undefined>((best, candidate) => {
              const distance = Math.abs(candidate.getBoundingClientRect().top - bounds.top - expected);
              return !best || distance < Math.abs(best.getBoundingClientRect().top - bounds.top - expected) ? candidate : best;
            }, undefined);
          if (row) {
            move(view, view.scrollTop + row.getBoundingClientRect().top - bounds.top - anchor.offset);
            anchored = true;
            break;
          }
        }
      }
      if (!anchored) move(view, last.top + (order === 'desc' ? view.scrollHeight - last.height : 0));
    }
    previous.current = position(view, scope);
  };

  useLayoutEffect(() => { reconcile.current(); }, [open, scope, order, entries, height, following]);
  useEffect(() => {
    const view = viewport.current;
    if (!open || !view || typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(() => reconcile.current());
    observer.observe(view);
    return () => observer.disconnect();
  }, [open, scope]);

  const onScroll = (view: HTMLElement) => {
    const top = boundedTop(view);
    if (programmaticTop.current != null && Math.abs(top - programmaticTop.current) <= 1) {
      previous.current = position(view, scope);
      return;
    }
    programmaticTop.current = null;
    const distance = order === 'asc' ? view.scrollHeight - view.clientHeight - top : top;
    changeFollowing(distance <= 2);
    previous.current = position(view, scope);
  };

  return { viewport, following, onScroll, toggleFollowing: () => changeFollowing(!follow.current) };
}
