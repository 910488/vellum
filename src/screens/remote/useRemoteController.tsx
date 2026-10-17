import { type WorkRemedy } from "@/lib/remoteWorkSurface";
import { useRemoteState } from './useRemoteState';
import { useRemoteConnection } from './useRemoteConnection';
import { useRemotePresentation } from './useRemotePresentation';
import { useRemoteActions } from './useRemoteActions';
import { useRemoteAccounts } from './useRemoteAccounts';
import { useRemotePolling } from './useRemotePolling';
import type { RemoteProps } from './useRemoteState';
export function useRemoteController(props: RemoteProps) {
  const state = useRemoteState(props);
  const connection = useRemoteConnection(state);
  const presentation = useRemotePresentation(connection);
  const actions = useRemoteActions(presentation);
  const accounts = useRemoteAccounts(actions);
  useRemotePolling(accounts);
  const {
    setPending,
    pairChatGpt,
    activateChatGpt,
    failure,
    retry,
    retryOperation,
  } = accounts;
  function runWorkRemedy(action: WorkRemedy) {
    switch (action) {
      case "stopAppOwned":
      case "installCodex":
      case "updateAgent":
      case "bootstrap":
      case "syncDesktopCodex":
        setPending(action);
        return;
      case "pairChatGpt":
        pairChatGpt();
        return;
      case "activateChatGpt":
        activateChatGpt();
        return;
      case "repair":
        setPending("bootstrap");
        return;
      case "plan": return;
      case "retry":
        if (failure)
          retry.current?.();
        else
          retryOperation();
        return;
      case "inspect": return;
    }
  }
  return {
    ...accounts,
    runWorkRemedy,
  };
}
export type RemoteController = ReturnType<typeof useRemoteController>;
