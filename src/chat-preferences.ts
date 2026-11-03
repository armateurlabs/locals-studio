const CONFIRM_DELETE_KEY = 'studio.chat.confirmDelete';

export function loadChatDeleteConfirmation(): boolean {
  try { return localStorage.getItem(CONFIRM_DELETE_KEY) !== 'false'; }
  catch { return true; }
}

export function saveChatDeleteConfirmation(confirm: boolean): void {
  try { localStorage.setItem(CONFIRM_DELETE_KEY, String(confirm)); }
  catch { /* Keep the preference usable for this session when storage is unavailable. */ }
}
