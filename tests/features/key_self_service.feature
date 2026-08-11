Feature: Stable key identity and read-only self-service statistics
  A user can read the same key history after credential rotation, but cannot administer the service.

  Scenario: Key rotation preserves history and policy without migration
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI upstream returns a successful completion
    When the service creates a key for principal "alice" allowing model "gpt-test"
    And the client calls model "gpt-test"
    Then the response status is 200
    When the service rotates the key
    Then the rotated credential retains the stable key id
    And the old credential is rejected
    When the client views its statistics with the rotated credential
    Then the statistics contain 1 request and 10 tokens
    And the request detail contains the archived prompt and response
    And the downstream key cannot create another key

  Scenario: Model permission is enforced before proxying
    Given a token center backed by SQLite and memory object storage
    When the service creates a key for principal "bob" allowing model "allowed-model"
    And the client calls model "denied-model"
    Then the response status is 403

  Scenario: Balance is reserved before calling the upstream
    Given a token center backed by SQLite and memory object storage
    When the service creates an exhausted key allowing model "paid-model"
    And the client calls model "paid-model"
    Then the response status is 429

  Scenario: Full-context agent requests share a logical conversation
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI upstream returns a successful completion
    When the service creates a key for principal "carol" allowing model "gpt-test"
    And the client sends two full-context requests for model "gpt-test" in one session
    Then the response status is 200
    And the requests form one logical conversation with a continuation edge

  Scenario: Claude Code can use the Anthropic Messages protocol
    Given a token center backed by SQLite and memory object storage
    And the mock Anthropic upstream returns a successful message
    When the service creates a key for principal "dave" allowing model "claude-test"
    And the Claude client calls model "claude-test"
    Then the response status is 200

  Scenario: RPM is enforced across requests
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI upstream returns a successful completion
    When the service creates a key with RPM 1 allowing model "gpt-test"
    And the client calls model "gpt-test" twice
    Then the response status is 429

  Scenario: API key and OAuth credentials share stable upstream routing
    Given a token center backed by SQLite and memory object storage
    And the mock routed upstream accepts API key and OAuth credentials
    When the service creates API and OAuth routes
    And the service creates a key allowing both routed models
    And the client calls both routed models
    Then the response status is 200
    And both upstream authentication types used the same routing pipeline
    When the service rotates the OAuth upstream credential
    And the client calls model "oauth-public"
    Then the response status is 200
    And the OAuth upstream account retains its stable id and uses generation 2

  Scenario: Cursor PKCE login and refresh create a routable OAuth account
    Given a token center backed by SQLite and memory object storage
    And the mock Cursor OAuth server and compatible upstream are ready
    When the service starts a Cursor OAuth login
    Then the Cursor login URL contains a PKCE challenge without exposing the verifier
    When the service polls the completed Cursor OAuth login
    And the service routes model "cursor-public" through the Cursor OAuth account
    And the service creates a key for principal "cursor-user" allowing model "cursor-public"
    And the client calls model "cursor-public"
    Then the response status is 200
    When the service refreshes the Cursor OAuth account
    And the client calls model "cursor-public"
    Then the response status is 200
    And the refreshed Cursor account keeps its id and uses generation 2
