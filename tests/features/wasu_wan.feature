Feature: Wasu Wan native video execution through MTC

  Scenario: Reference image generation reuses MTC jobs, assets and seconds settlement
    Given a token center backed by SQLite and memory object storage
    And the mock Wasu Wan upstream completes a reference image video
    When the service creates a second-priced Wasu Wan video route and key
    And the client creates and replays a Wasu Wan reference image generation
    Then the response status is 202
    And the Wasu Wan video is delivered through MTC once with seconds billing

  Scenario: An unknown supplier acknowledgement never causes another submit
    Given a token center backed by SQLite and memory object storage
    And the mock Wasu Wan upstream returns an unknown submit acknowledgement
    When the service creates a second-priced Wasu Wan video route and key
    And the client creates and replays a Wasu Wan reference image generation
    Then the response status is 202
    And the Wasu Wan unknown acknowledgement is quarantined without resubmission
