Feature: Native token-priced tkmarket video through the MTC API

  Scenario Outline: Exact models deliver bytes and settle the immutable customer price once
    Given a token center backed by SQLite and memory object storage
    When the service configures tkmarket model "<model>" with terminal "<terminal>"
    And the client creates and replays a tkmarket video before the customer price changes
    Then the tkmarket job retains "<terminal>" usage and one independent customer settlement

    Examples:
      | model                | terminal       |
      | seedance-2.0-idle-ts | success        |
      | seedance-2.5-idle-ts | success        |
      | seedance-2.0-idle-ts | failure        |
      | seedance-2.0-idle-ts | unknown-cost   |
      | seedance-2.0-idle-ts | missing-tokens |
      | seedance-2.0-idle-ts | missing-asset  |
      | seedance-2.0-idle-ts | wrong-task     |
      | seedance-2.0-idle-ts | unknown-ack    |
