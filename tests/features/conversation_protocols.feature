Feature: Logical conversations across supported coding clients and protocols
  Protocol adapters preserve explicit ancestry and do not let utility requests pollute conversations.

  Scenario: Buffered Responses previous_response_id links directly to its parent response
    Given a token center backed by SQLite and memory object storage
    And the mock buffered Responses upstream returns parent and child responses
    When the service creates a key for principal "responses-buffered" allowing model "gpt-test"
    And the Responses client sends a buffered parent and child for model "gpt-test"
    Then the response status is 200
    And the two Responses requests have a direct continuation edge

  Scenario: Streaming Responses previous_response_id links directly to its parent response
    Given a token center backed by SQLite and memory object storage
    And the mock streaming Responses upstream returns parent and child events
    When the service creates a key for principal "responses-streaming" allowing model "gpt-test"
    And the Responses client sends a streaming parent and child for model "gpt-test"
    Then the response status is 200
    And the two Responses requests have a direct continuation edge

  Scenario: A failed streaming Responses id cannot become a lineage parent
    Given a token center backed by SQLite and memory object storage
    And the mock failed streaming Responses upstream returns parent and child events
    When the service creates a key for principal "responses-failed" allowing model "gpt-test"
    And the Responses client sends a failed streaming parent and child for model "gpt-test"
    Then the response status is 200
    And the failed Responses id does not form a continuation edge

  Scenario: Delivered streaming output with invalid usage is charged to its admitted ceiling
    Given a token center backed by SQLite and memory object storage
    And the mock streaming Responses upstream exceeds the admitted usage
    When the service creates a key for principal "responses-invalid-usage" allowing model "gpt-test"
    And the Responses client consumes the invalid usage stream for model "gpt-test"
    Then the delivered invalid stream is a fully billed failure without response lineage

  Scenario: Consecutive compactions and a later branch retain their ancestry
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI upstream returns a successful completion
    When the service creates a key for principal "compaction-chain" allowing model "gpt-test"
    And the client sends two consecutive compactions followed by a branch for model "gpt-test"
    Then the response status is 200
    And the conversation contains two compaction edges followed by a branch edge

  Scenario: Anthropic metadata and beta headers preserve Claude Code ancestry
    Given a token center backed by SQLite and memory object storage
    And the mock Anthropic upstream requires metadata and a beta header
    When the service creates a key for principal "claude-metadata" allowing model "claude-test"
    And the Claude client sends metadata-linked turns for model "claude-test"
    Then the response status is 200
    And the Anthropic turns have a direct compaction edge

  Scenario: Embeddings and token counting do not create conversation observations
    Given a token center backed by SQLite and memory object storage
    And the mock utility and OpenAI upstreams return successful responses
    When the service creates a key for principal "utility-user" allowing model "gpt-test"
    And the client sends chat, embedding, and token counting requests for model "gpt-test"
    Then the response status is 200
    And only the chat request appears in logical conversations

  Scenario: WorkBuddy uses OpenAI chat with max_completion_tokens and API key auth
    Given a token center backed by SQLite and memory object storage
    And the mock WorkBuddy OpenAI upstream requires max_completion_tokens
    When the service creates a key for principal "workbuddy-user" allowing model "gpt-test"
    And WorkBuddy sends two OpenAI chat turns for model "gpt-test"
    Then the response status is 200
    And the WorkBuddy requests form one logical conversation

  Scenario: Explicit header and body markers create paginated subagent ancestry
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI upstream returns a successful completion
    When the service creates a key for principal "explicit-subagents" allowing model "gpt-test"
    And the client sends a parent with header-marked and body-marked subagents for model "gpt-test"
    Then the response status is 200
    And the paginated conversation exposes two explicit subagent edges

  Scenario: Client vocabulary and orphan markers never imply subagent ancestry
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI upstream returns a successful completion
    When the service creates a key for principal "implicit-subagents" allowing model "gpt-test"
    And the client sends UA branch and orphan subagent hints for model "gpt-test"
    Then the response status is 200
    And no logical conversation contains a subagent edge
