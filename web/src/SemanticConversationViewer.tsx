import { useMemo } from 'react';
import { MessageBar, MessageBarBody } from '@fluentui/react-components';
import { MemeLoopRuntimeProvider, MemeLoopThread, type WebMemeLoopChatAdapter, type MemeLoopThreadProps } from '@memeloop/react-ui/chat';

const EmptyComposer = () => null;
const NoTurnActions = () => null;

type WriteCapability = 'sendMessage' | 'cancel' | 'deleteTurn' | 'retryTurn' | 'editMessage' | 'editing'
  | 'resolveAskQuestion' | 'updateMessage' | 'executionTargets' | 'activeExecutionTarget' | 'setExecutionTarget';

/** Host projection of the single semantic reader. No raw archive loader enters
 * this boundary. Pagination, reasoning, attachments and exports stay host-owned.
 */
export type SemanticConversationAdapter = Omit<WebMemeLoopChatAdapter, WriteCapability>;

export interface SemanticCoverage {
  observed_terminal: boolean;
  projection_state: 'complete' | 'partial';
  omissions: readonly string[];
  gaps: readonly string[];
}

export function SemanticConversationViewer({ adapter, revision, readOnlyMessage, coverage, incompleteMessage, ...threadProps }: {
  adapter: SemanticConversationAdapter;
  revision: string;
  readOnlyMessage: string;
  coverage: SemanticCoverage;
  incompleteMessage: string;
} & Pick<MemeLoopThreadProps, 'header' | 'empty' | 'messageLabels' | 'timelineLabels' | 'renderMessageContent' | 'toolResultRenderers' | 'renderOperationError'>) {
  const readonlyAdapter = useMemo<WebMemeLoopChatAdapter>(() => {
    const rejectWrite = async () => { throw new Error(readOnlyMessage); };
    return { ...adapter, sendMessage: rejectWrite, cancel: rejectWrite, deleteTurn: rejectWrite, retryTurn: rejectWrite,
      editMessage: undefined, editing: undefined, resolveAskQuestion: undefined, updateMessage: undefined,
      executionTargets: undefined, activeExecutionTarget: undefined, setExecutionTarget: undefined };
  }, [adapter, readOnlyMessage]);
  return <MemeLoopRuntimeProvider key={JSON.stringify([adapter.conversationId, revision])} adapter={readonlyAdapter}>
    {(coverage.projection_state !== 'complete' || !coverage.observed_terminal || coverage.gaps.length > 0 || coverage.omissions.length > 0) && <MessageBar intent="warning"><MessageBarBody>{incompleteMessage}<ul>{[...coverage.gaps, ...coverage.omissions].map((reason, index) => <li key={index}>{reason}</li>)}</ul></MessageBarBody></MessageBar>}
    <MemeLoopThread {...threadProps} composerComponent={EmptyComposer} renderTurnActions={NoTurnActions}
      attachmentRevision={revision} showTimeline />
  </MemeLoopRuntimeProvider>;
}
