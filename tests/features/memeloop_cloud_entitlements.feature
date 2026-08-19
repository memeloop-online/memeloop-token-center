Feature: MemeLoop Cloud subscription entitlement synchronization
  Registration and subscription events are authenticated, replay-safe, ordered, and attached to stable history.

  Scenario: Signed subscription lifecycle preserves stable identity and rejects rollback
    Given a token center backed by SQLite and memory object storage
    When MemeLoop Cloud signs an initial subscription snapshot
    Then the Cloud snapshot creates one stable credential with exact quota and policy
    When MemeLoop Cloud retries the same signed event
    Then the Cloud retry does not duplicate credit or credential history
    When MemeLoop Cloud applies version 3 and then delivers version 2
    Then the stale Cloud event is rejected without rolling quota or policy back
    When MemeLoop Cloud signs a newer cancellation snapshot
    Then only the unconsumed subscription remainder is withdrawn
