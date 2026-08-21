Feature: Stable logical sessions and authoritative usage
  Logical sessions remain key-scoped, cursor-stable, filterable before pagination,
  and keep archive-only diagnostics separate from billable usage.

  Scenario: Credential rotation preserves stable logical-session history
    Given a logical session token center backed by SQLite
    Then rotated credentials retain logical-session history without granting another credential access

  Scenario: Operator logical-session filters run before the recent limit
    Given a logical session token center backed by SQLite
    Then every operator logical-session filter finds its match beyond the first fifty sessions

  Scenario: Logical-session cursors totally order equal activity timestamps
    Given a logical session token center backed by SQLite
    Then equal logical-session activity timestamps paginate without duplicates or omissions

  Scenario: Archive-only sessions are diagnostic rather than billable
    Given a logical session token center backed by SQLite
    Then archive-only logical-session metrics do not change authoritative usage or cost

  Scenario: Usage analysis keeps session identity and currencies separate
    Given a logical session token center backed by SQLite
    Then usage analysis sessions expose key identity and never combine USD with CNY

  @postgres
  Scenario: PostgreSQL satisfies the complete logical-session black-box contract
    Given a logical session token center backed by PostgreSQL
    Then every logical-session acceptance contract holds
