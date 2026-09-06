Feature: Production authorization and credential continuity
  The control plane must enforce tenant boundaries, and credential rotation must not split
  identity or history.

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
