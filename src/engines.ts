import { ENGINE_LABELS, ENGINES, type Engine, type EngineStatus, type ModelRecord, type Profile, type Server, type Snapshot } from './types';

/** A view's engine filter: every engine, or one. */
export type EngineFilter = 'all' | Engine;

/** Profiles and servers saved before engines existed run on Locals. */
export const engineOf = (item: Pick<Profile, 'engine'> | Pick<Server, 'engine'> | undefined): Engine => item?.engine ?? 'locals';

/** The engines that can load a model; older records were checked for Locals only. */
export const modelEngines = (model: Pick<ModelRecord, 'engines' | 'compatible'>): Engine[] =>
  model.engines ?? (model.compatible ? ['locals'] : []);

export const engineLabel = (engine: Engine) => ENGINE_LABELS[engine] ?? engine;

export const matchesEngine = (filter: EngineFilter, engine: Engine) => filter === 'all' || filter === engine;

export const engineStatus = (snapshot: Snapshot, engine: Engine): EngineStatus | undefined => snapshot.engines?.[engine];

export const installedEngines = (snapshot: Snapshot): Engine[] =>
  ENGINES.filter(engine => snapshot.engines?.[engine]?.installed);

/** A running server started on another version than the one now installed picks it up on restart. */
export function restartRequired(server: Server, snapshot: Snapshot): boolean {
  if (!server.runtime_version || !['ready', 'running', 'starting', 'loading'].includes(server.state)) return false;
  const installed = snapshot.engines?.[engineOf(server)]?.version;
  return !!installed && installed !== server.runtime_version;
}
