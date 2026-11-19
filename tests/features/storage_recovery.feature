@postgres @storage-recovery
Feature: Original bodies remain available through recovery
  Scenario: Temporary object publication failure preserves the delivered response and original bodies
    Given a recovery service backed by PostgreSQL and real object storage
    And the mock OpenAI upstream returns a successful completion
    When the service creates a key for principal "publication-recovery" allowing model "gpt-test"
    Given object publication for this customer temporarily returns service unavailable
    When the client calls model "gpt-test"
    Then the original HTTP response succeeds while publication has failed
    When object publication recovers
    Then both customer archive bodies recover exactly and billing occurs once
