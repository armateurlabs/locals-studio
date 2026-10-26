import type { JsonObject, Snapshot } from './types';

export function supportsWorkerActivity(service: JsonObject | undefined): boolean {
  return Array.isArray(service?.capabilities) && service.capabilities.includes('worker_activity_v1');
}

export function serviceUpdatePending(snapshot: Snapshot): boolean {
  if (snapshot.service?.upgrade?.status === 'pending') return true;
  if (Array.isArray(snapshot.service?.capabilities)) return !supportsWorkerActivity(snapshot.service);
  // Older services did not advertise capabilities. A worker without the activity
  // field identifies their schema; a modern unavailable sample still has it.
  return snapshot.servers.some(server => server.pid != null && ['ready', 'running'].includes(server.state)
    && (snapshot.telemetry.workers ?? []).some((worker: JsonObject) => worker.profile_id === server.profile_id && worker.pid === server.pid && worker.activity == null));
}

export function mergeServiceInfo(previous: JsonObject | undefined, incoming: JsonObject | undefined) {
  // During one service connection, an older in-flight read cannot remove a
  // capability that has already been observed. Reconnection resets this floor.
  return supportsWorkerActivity(previous) && !supportsWorkerActivity(incoming) ? previous : incoming;
}
