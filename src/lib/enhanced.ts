export interface EnhancedSession {
  threadId: string;
  parentThreadId: string | null;
  plane: string;
  model: string | null;
  state: string;
  lastActivity: number;
}
export interface RemoteObservation {
  state: string;
  ownerPid: number | null;
  clients: Record<string, string>;
  handshakeObserved: boolean;
  listObserved: boolean;
  historyObserved: boolean;
  streamObserved: boolean;
  controlObserved: boolean;
  lastFailureStage: string | null;
  lastErrorCode: number | null;
}
export interface RuntimeObservations {
  schemaVersion: number;
  launchId: string;
  bridgePid: number;
  updatedAt: number;
  freshness: string;
  sessions: Record<string, EnhancedSession>;
  remote: RemoteObservation;
}
export const EMPTY_OBSERVATIONS: RuntimeObservations = {
  schemaVersion: 1, launchId: "", bridgePid: 0, updatedAt: 0, freshness: "unavailable", sessions: {},
  remote: { state: "unavailable", ownerPid: null, clients: {}, handshakeObserved: false,
    listObserved: false, historyObserved: false, streamObserved: false, controlObserved: false,
    lastFailureStage: null, lastErrorCode: null },
};
export function activeEnhancedSessions(report: RuntimeObservations) {
  if (report.freshness !== "current") return [];
  return Object.values(report.sessions).filter(row => row.state !== "unloaded");
}
