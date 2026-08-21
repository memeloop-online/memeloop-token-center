Feature: Black-box credential policy and tenant isolation acceptance
  Public and control-plane authorization must fail closed before parsing attacker
  input, and stable credentials must preserve strict status and quota semantics.

  Scenario: An empty model allowlist is deny-all
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI upstream returns a successful completion
    Then a credential with an empty model allowlist cannot list or call a priced model

  Scenario: Managed downstream credential states are enforced and revocation is terminal
    Given a token center backed by SQLite and memory object storage
    Then a managed credential can be suspended and restored but never restored after revocation

  Scenario: Managed service credentials are global-only and revocation is terminal
    Given a token center backed by SQLite and memory object storage
    Then service credential status management requires a global operator and enforces terminal revocation

  Scenario: Every documented service scope authorizes only its operation family
    Given a token center backed by SQLite and memory object storage
    Then each service scope independently authorizes its matching control-plane operation

  Scenario: OAuth initiation validates tenant and upstream network boundaries
    Given a token center backed by SQLite and memory object storage
    Then tenant scoped OAuth cannot target private or metadata endpoints while a global private connection is allowed

  Scenario: Provider JSON Schemas reject invalid and undeclared values
    Given a token center backed by SQLite and memory object storage
    Then provider configuration and credential schemas are authoritative on every write

  Scenario: Authentication precedes parsing and request size enforcement
    Given a token center backed by SQLite and memory object storage
    Then malformed and oversized unauthenticated bodies are rejected as unauthorized

  Scenario: Self-service filters generation jobs and conversations cannot cross credentials
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI upstream returns a successful completion
    When the service prepares two tenants and credentials for the authorization matrix
    Then self-service object and filter access remains bound to the authenticated credential

  Scenario: Credit grants replay exactly and reject changed payloads
    Given a token center backed by SQLite and memory object storage
    Then a grant idempotency key replays the same payload and conflicts on a changed payload

  Scenario: TPM concurrency and all budget windows reject excess reservations
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI upstream returns a successful completion
    Then TPM concurrency daily weekly and lifetime policies are independently enforced

  Scenario: Group routing exclusions overlap and credential rotation preserve authorization
    Given a token center backed by SQLite and memory object storage
    When the operator configures overlapping provider groups and two route groups
    Then provider exclusions win and overlapping route group grants are deduplicated
    When the operator rotates the group-routed credential
    Then the rotated credential preserves its route and route-group authorization
