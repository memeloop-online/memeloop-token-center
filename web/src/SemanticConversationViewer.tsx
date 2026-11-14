import { useMemo } from 'react';
import { MemeLoopRuntimeProvider, MemeLoopThread, type WebMemeLoopChatAdapter, type MemeLoopThreadProps } from '@memeloop/react-ui/chat';

const EmptyComposer = () => null;
const NoTurnActions = () => null;

type WriteCapability = 'sendMessage' | 'cancel' | 'deleteTurn' | 'retryTurn' | 'editMessage' | 'editing'
  | 'resolveAskQuestion' | 'updateMessage' | 'executionTargets' | 'activeExecutionTarget' | 'setExecutionTarget';

/** Host projection of the single semantic reader. No raw archive loader enters
 * this boundary. Pagination, reasoning, attachments and exports stay host-owned.
 */
export type SemanticConversationAdapter = Omit<WebMemeLoopChatAdapter, WriteCapability>;

export function SemanticConversationViewer({ adapter, revision, readOnlyMessage, ...threadProps }: {
  adapter: SemanticConversationAdapter;
  revision: string;
  readOnlyMessage: string;
} & Pick<MemeLoopThreadProps, 'header' | 'empty' | 'messageLabels' | 'timelineLabels' | 'renderMessageContent' | 'toolResultRenderers' | 'renderOperationError'>) {
  const readonlyAdapter = useMemo<WebMemeLoopChatAdapter>(() => {
    const rejectWrite = async () => { throw new Error(readOnlyMessage); };
    return { ...adapter, sendMessage: rejectWrite, cancel: rejectWrite, deleteTurn: rejectWrite, retryTurn: rejectWrite,
      editMessage: undefined, editing: undefined, resolveAskQuestion: undefined, updateMessage: undefined,
      executionTargets: undefined, activeExecutionTarget: undefined, setExecutionTarget: undefined };
  }, [adapter, readOnlyMessage]);
  return <MemeLoopRuntimeProvider key={JSON.stringify([adapter.conversationId, revision])} adapter={readonlyAdapter}>
    <MemeLoopThread {...threadProps} composerComponent={EmptyComposer} renderTurnActions={NoTurnActions}
      attachmentRevision={revision} showTimeline />
  </MemeLoopRuntimeProvider>;
}
