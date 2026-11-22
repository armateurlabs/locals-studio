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
export interface ModelAlias { id: string; name: string; profile_ids: string[]; }
export interface ConfigurationPreset { id: string; name: string; options: JsonObject; read_only?: boolean; }
export interface Server {
  profile_id: string; alias: string; model_id: string; state: string; pid?: number;
  port?: number; started_at?: string; backend_model?: string; backend_models?: string[]; error?: string; active_requests: number;
}
export interface Download {
  id: string; repo_id: string; revision?: string; state?: string; status?: string;
  downloaded_bytes?: number; total_bytes?: number; progress?: number; error?: string;
  bytes_per_second?: number; eta_seconds?: number;
  transport?: string; replayed_bytes?: number;
}
export interface Folder { id: string; name: string; }
export interface Message { id?: string; role: string; content: string; model?: string; reasoning_content?: string; usage?: JsonObject; finish_reason?: string | null; output_token_limit?: number | null; routed_profile_id?: string | null; routed_model_id?: string | null; routed_alias?: string | null; cancelled?: boolean; error?: string; }
export interface Conversation {
  id: string; title: string; folder_id?: string | null; messages: Message[];
  model?: string; thinking?: boolean; updated_at?: string;
  generation_parameters?: JsonObject;
}
export interface NewChatRequest { id: string; model: string; }
export interface Snapshot {
  config: JsonObject; models: ModelRecord[]; profiles: Profile[]; servers: Server[];
  downloads: Download[]; runtime: JsonObject; telemetry: JsonObject;
  folders: Folder[]; conversations: Conversation[];
  aliases?: ModelAlias[];
  configuration_presets?: ConfigurationPreset[];
  api?: JsonObject; service?: JsonObject;
}
export interface StudioEvent { type: string; data: JsonObject; }
export interface UsageDay {
  requests: number; prompt: number; cached: number; output: number; total: number;
  models: Record<string, number>; clients: Record<string, number>;
}
export interface UsageSummary {
  days: Record<string, UsageDay>;
  all: UsageDay;
  active_days: number;
  busiest_day: { date: string; usage: UsageDay } | null;
}
export const EMPTY_SNAPSHOT: Snapshot = {
  config: {}, models: [], profiles: [], servers: [], downloads: [], runtime: {},
  telemetry: {}, folders: [], conversations: [], aliases: [], configuration_presets: [],
};
