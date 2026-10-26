export type JsonObject = Record<string, any>;
export interface ModelRecord {
  id: string; name: string; source: string; repo_id?: string; revision?: string;
  path: string; weights_bytes: number; compatible: boolean; architecture?: string;
  quantization?: string; context_length?: number;
}
export interface Profile {
  id: string; model_id: string; alias: string; selected: boolean;
  options: JsonObject; env: Record<string, string>;
}
export interface Server {
  profile_id: string; alias: string; model_id: string; state: string; pid?: number;
  port?: number; started_at?: string; backend_model?: string; error?: string; active_requests: number;
}
export interface Download {
  id: string; repo_id: string; revision?: string; state?: string; status?: string;
  downloaded_bytes?: number; total_bytes?: number; progress?: number; error?: string;
  bytes_per_second?: number; eta_seconds?: number;
  transport?: string; replayed_bytes?: number;
}
export interface Folder { id: string; name: string; }
export interface Message { id?: string; role: string; content: string; reasoning_content?: string; usage?: JsonObject; cancelled?: boolean; error?: string; }
export interface Conversation {
  id: string; title: string; folder_id?: string | null; messages: Message[];
  model?: string; updated_at?: string;
}
export interface Snapshot {
  config: JsonObject; models: ModelRecord[]; profiles: Profile[]; servers: Server[];
  downloads: Download[]; runtime: JsonObject; telemetry: JsonObject;
  folders: Folder[]; conversations: Conversation[];
  api?: JsonObject;
}
export interface StudioEvent { type: string; data: JsonObject; }
export const EMPTY_SNAPSHOT: Snapshot = {
  config: {}, models: [], profiles: [], servers: [], downloads: [], runtime: {},
  telemetry: {}, folders: [], conversations: [],
};
