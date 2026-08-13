Feature: Production authorization, credential continuity, and CPA migration acceptance
  The control plane must enforce tenant boundaries, credential rotation must not split
  identity or history, and repeated CPA imports must remain lossless and idempotent.

  Scenario: Global, tenant-scoped, and downstream credentials cannot cross authority boundaries
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI upstream returns a successful completion
    When the service prepares two tenants and credentials for the authorization matrix
    Then the global service credential lists both tenants and reads both request details
    And the tenant scoped service credential lists and reads only its own tenant
    And the tenant scoped service credential cannot read another tenant or synchronize global prices
    And the downstream credential cannot administer the service or read another credential history

  Scenario: Credential status and rotation preserve stable identity policy balance and history
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI upstream returns a successful completion
    When the service creates and uses a credential with an explicit policy and budget
    And the service suspends and reactivates that credential
    And the service rotates the key
    Then the rotated credential retains stable identity policy balance and history
    And the old credential is rejected

  @postgres
  Scenario: CPAMP incremental import is idempotent and includes late overlap events
    Given a migrated PostgreSQL schema and a CPAMP SQLite fixture
    When the CPAMP importer runs twice over the initial fixture
    Then the imported requests aggregates and checkpoint contain exactly the initial events
    When a late overlap event and a newer event are appended and the importer runs twice
    Then the imported requests aggregates and checkpoint contain every event exactly once
