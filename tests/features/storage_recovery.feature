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

  Scenario: Legacy bound archives preserve request and compressed incomplete response originals
    Given a recovery service backed by PostgreSQL and real object storage
    When the service creates a key for principal "legacy-bound-recovery" allowing model "gpt-test"
    Given an authorized legacy bound request has a compressed response and no raw root
    Then the customer archive API returns both legacy originals byte for byte

  Scenario: Blocked historical coverage recovery allows fresh HTTP response and body publication
    Given a recovery service backed by PostgreSQL and real object storage
    And the mock OpenAI upstream returns a successful completion
    When the service creates a key for principal "historical-coverage-recovery" allowing model "gpt-test"
    Given historical coverage receipt publication for this customer is unavailable
    When a historical request reaches blocked coverage recovery
    Then a fresh request succeeds with readable originals and one bill per request
    When historical coverage recovery is released
    Then historical coverage recovers without another bill
