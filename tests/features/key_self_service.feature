Feature: Stable key identity and read-only self-service statistics
  A user can read the same key history after credential rotation, but cannot administer the service.

  Scenario: Key rotation preserves history and policy without migration
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI upstream returns a successful completion
    When the service creates a key for principal "alice" allowing model "gpt-test"
    And the client calls model "gpt-test"
    Then the response status is 200
    And the operator realtime stream contains started and finished events
    When the service rotates the key
    Then the rotated credential retains the stable key id
    And the old credential is rejected
    When the client views its statistics with the rotated credential
    Then the statistics contain 1 request and 10 tokens
    And the request detail contains the archived prompt and response
    And the downstream key cannot create another key

  Scenario: Alias rename preserves stable identity and exposes the key's own limits
    Given a token center backed by SQLite and memory object storage
    When the service creates a key for principal "alias-user" allowing model "gpt-test"
    And the service renames the key alias to "renamed credential"
    Then the renamed alias retains the stable key identity
    When the client views its own limit snapshot
    Then the own limit snapshot belongs to the stable key

  Scenario: A global operator credential sees imported-style history across tenants
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI upstream returns a successful completion
    When the service records requests for tenants "first-import" and "second-import"
    Then global operator statistics contain both tenant requests
    And tenant filtered operator statistics contain only "first-import"

  Scenario: Model permission is enforced before proxying
    Given a token center backed by SQLite and memory object storage
    When the service creates a key for principal "bob" allowing model "allowed-model"
    And the client calls model "denied-model"
    Then the response status is 403

  Scenario: An imported opaque key keeps its exact credential and historical identity
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI upstream returns a successful completion
    When the service creates a key for principal "legacy-linux" allowing model "gpt-test"
    And the service installs an imported opaque CPA key
    And the client calls model "gpt-test"
    Then the response status is 200
    When the client views statistics with the imported opaque CPA key
    Then the statistics contain 1 request and 10 tokens
    When the service rotates the key
    Then the old credential is rejected

  Scenario: Balance is reserved before calling the upstream
    Given a token center backed by SQLite and memory object storage
    When the service creates an exhausted key allowing model "paid-model"
    And the client calls model "paid-model"
    Then the response status is 429
    And the rejection reason is "balance_exhausted" and is not retryable

  Scenario: Subscription grant reversal is durable and idempotent
    Given a token center backed by SQLite and memory object storage
    When the service creates and grants an unspent subscription key
    And the service reverses that subscription grant twice
    Then the subscription balance is zero after one logical reversal

  Scenario: Full-context agent requests share a logical conversation
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI upstream returns a successful completion
    When the service creates a key for principal "carol" allowing model "gpt-test"
    And the client sends two full-context requests for model "gpt-test" in one session
    Then the response status is 200
    And the requests form one logical conversation with a continuation edge

  Scenario: Explicit turn ancestry survives client-side context compaction
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI upstream returns a successful completion
    When the service creates a key for principal "erin" allowing model "gpt-test"
    And the client sends a parent-linked compacted turn for model "gpt-test"
    Then the response status is 200
    And the compacted request is linked to its explicit parent turn

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
    And the rejection reason is "rpm_exhausted" and is retryable with Retry-After

  Scenario: Cached tokens and service tiers are charged from one immutable snapshot
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI Responses upstream returns cached priority usage
    When the service creates a key for principal "cache-tier-user" allowing model "cache-tier-model"
    And the operator configures cache-aware default and priority prices for model "cache-tier-model"
    And the client calls priority Responses model "cache-tier-model"
    Then the response status is 200
    And the cache-aware priority request costs 0.00046 for 120 tokens

  Scenario: Cache writes are charged through the public Anthropic HTTP endpoint
    Given a token center backed by SQLite and memory object storage
    And the mock Anthropic upstream returns cache write usage
    When the service creates a key for principal "cache-write-user" allowing model "claude-cache-write-model"
    And the operator configures cache-aware default and priority prices for model "claude-cache-write-model"
    And the Claude client calls model "claude-cache-write-model"
    Then the response status is 200
    And the cache-writing request costs 0.000089 for 102 tokens

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
    When the service starts reauthorization for the Cursor OAuth account
    And the service polls the completed Cursor OAuth reauthorization
    And the client calls model "cursor-public"
    Then the response status is 200
    And the reauthorized Cursor account keeps its id and route and uses generation 3

  Scenario: Scoped service credentials rotate without crossing tenant boundaries
    Given a token center backed by SQLite and memory object storage
    When the bootstrap service creates a tenant scoped service token
    Then the scoped service token can create a key in its tenant
    And the scoped service token cannot update global prices
    When the bootstrap service rotates the scoped service token
    Then the old service token is rejected
    And the rotated service token retains its stable service id

  Scenario: Seedance generation is permissioned, metered, polled and archived
    Given a token center backed by SQLite and memory object storage
    And the mock Seedance upstream completes a five second video
    When the service creates a metered Seedance route and key
    And the client creates a five second Seedance generation
    Then the response status is 202
    And the generation eventually succeeds with an archived video costing 0.5

  Scenario: SiliconFlow text to video reuses the shared HTTP JSON account safely
    Given a token center backed by SQLite and memory object storage
    And the mock SiliconFlow upstream completes a text to video request
    When the service creates a job-priced SiliconFlow video route and key
    And the client creates and replays a SiliconFlow text to video generation
    Then the response status is 202
    And the SiliconFlow video is archived once with safe metadata and job billing

  Scenario: A running Seedance job is cancelled upstream and refunded exactly once
    Given a token center backed by SQLite and memory object storage
    And the mock Seedance upstream keeps a generation running until cancellation
    When the service creates a metered Seedance route and key
    And the client creates a five second Seedance generation
    Then cancelling the running Seedance generation is idempotent and refunds exactly once

  Scenario: A permanently rejected Seedance generation is refunded without retries
    Given a token center backed by SQLite and memory object storage
    And the mock Seedance upstream rejects the generation request
    When the service creates a metered Seedance route and key
    And the client creates a five second Seedance generation
    Then the response status is 202
    And the rejected generation fails once and refunds its entire reservation

  Scenario: A Seedance success without a video asset is sanitized and refunded
    Given a token center backed by SQLite and memory object storage
    And the mock Seedance upstream reports success without a video asset
    When the service creates a metered Seedance route and key
    And the client creates a five second Seedance generation
    Then the response status is 202
    And the assetless Seedance success fails safely and refunds its entire reservation

  Scenario: A malicious upstream generation job id is bounded and never exposed
    Given a token center backed by SQLite and memory object storage
    And the mock Seedance upstream returns a malicious job id
    When the service creates a metered Seedance route and key
    And the client creates a five second Seedance generation
    Then the response status is 202
    And the malicious Seedance job id is neither stored nor exposed

  Scenario: An ambiguous non-idempotent Seedance submission fails closed without a duplicate
    Given a token center backed by SQLite and memory object storage
    And the mock Seedance upstream returns an ambiguous server error after one submission
    When the service creates a metered Seedance route and key
    And the client creates a five second Seedance generation
    Then the response status is 202
    And the ambiguous Seedance submission fails closed without a second upstream POST

  Scenario: Seedance provider usage cannot exceed the admitted reservation
    Given a token center backed by SQLite and memory object storage
    And the mock Seedance upstream reports sixty seconds for a five second reservation
    When the service creates a metered Seedance route and key
    And the client creates a five second Seedance generation
    Then the response status is 202
    And the over-contract Seedance usage charges the reservation ceiling without an asset

  Scenario: ComfyUI generation is permissioned, metered and archived
    Given a token center backed by SQLite and memory object storage
    And the mock ComfyUI upstream completes an image workflow
    When the service creates a metered ComfyUI route and key
    And the client creates a ComfyUI image generation
    Then the response status is 202
    And the ComfyUI generation eventually succeeds with an archived image costing 0.2

  Scenario: A running ComfyUI job is fenced, cancelled upstream and refunded exactly once
    Given a token center backed by SQLite and memory object storage
    And the mock ComfyUI upstream keeps a generation running until cancellation
    When the service creates a metered ComfyUI route and key
    And the client creates a ComfyUI image generation
    Then cancelling the running ComfyUI generation is idempotent and refunds exactly once

  Scenario: ComfyUI megapixel billing settles actual output pixels and refunds unused reservation
    Given a token center backed by SQLite and memory object storage
    And the mock ComfyUI upstream returns two images for a three image request
    When the service creates a megapixel-priced ComfyUI route and key
    And the client creates a three-output ComfyUI megapixel generation
    Then the ComfyUI generation bills exactly 1.048576 megapixels and refunds the unused output

  Scenario: ComfyUI image admission enforces every credential limit before upstream work
    Given a token center backed by SQLite and memory object storage
    Then the asynchronous ComfyUI image endpoint rejects permission quota RPM TPM and concurrency violations

  Scenario: Seedance video admission enforces every credential limit before upstream work
    Given a token center backed by SQLite and memory object storage
    Then the asynchronous Seedance video endpoint rejects permission quota RPM TPM and concurrency violations

  Scenario: A durable generation manifest survives a worker crash before terminal settlement
    Given a token center backed by SQLite and memory object storage
    When the service creates a metered ComfyUI route and key
    And the generation worker is stopped before it can submit upstream
    And the client creates a ComfyUI image generation
    And a durable ComfyUI manifest is persisted before terminal settlement
    Then the restarted worker settles the durable manifest without contacting ComfyUI

  Scenario: A ComfyUI success without generated assets is sanitized and refunded
    Given a token center backed by SQLite and memory object storage
    And the mock ComfyUI upstream reports success without generated assets
    When the service creates a metered ComfyUI route and key
    And the client creates a ComfyUI image generation
    Then the response status is 202
    And the assetless ComfyUI success fails safely and refunds its entire reservation

  Scenario: An oversized ComfyUI asset manifest is rejected before any download
    Given a token center backed by SQLite and memory object storage
    And the mock ComfyUI upstream returns seventeen generated assets
    When the service creates a metered ComfyUI route and key
    And the client creates a ComfyUI image generation
    Then the response status is 202
    And the oversized ComfyUI manifest fails before downloads and refunds its reservation

  Scenario: ComfyUI video generation uses the video endpoint, job billing and job-scoped durable archive
    Given a token center backed by SQLite and memory object storage
    And the mock ComfyUI upstream completes an MP4 video workflow
    When the service creates a metered ComfyUI video route and key
    And the client creates a ComfyUI video generation
    Then the response status is 202
    And the ComfyUI video is available through self service with exact archived content and cost 0.2

  Scenario: OpenAI-compatible image generation is forwarded and metered
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI Images upstream returns a generated icon
    When the service creates a metered OpenAI Images route and key
    And the client creates an OpenAI-compatible image
    Then the response status is 200
    And the OpenAI image response is archived and costs 0.3

  Scenario: OpenAI-compatible image generation is atomic without an idempotency key
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI Images upstream returns a generated icon without requiring idempotency
    When the service creates a metered OpenAI Images route and key
    And the client creates an OpenAI-compatible image without an idempotency key
    Then the response status is 200
    And the non-idempotent OpenAI image is atomically archived and costs 0.3

  Scenario: URL-backed OpenAI image results are durably archived
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI Images upstream returns an exact-origin signed URL
    When the service creates a metered OpenAI Images route and key
    And the client creates an OpenAI-compatible image
    Then the response status is 200
    And the signed URL image is stored in CAS without exposing its secret URL

  Scenario: Ten OpenAI image results share one aggregate archive budget
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI Images upstream returns ten assets over the aggregate budget
    When the service creates a metered OpenAI Images route and key
    And the client creates ten OpenAI-compatible images in one request
    Then the response status is 502
    And the ten image request is refunded and leaves no staged assets

  Scenario: Empty URL-backed OpenAI image results are rejected without billing
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI Images upstream returns an empty signed URL asset
    When the service creates a metered OpenAI Images route and key
    And the client creates an OpenAI-compatible image
    Then the response status is 502
    And the empty URL image is rejected unbilled without exposing the signed URL

  Scenario: Oversized OpenAI image responses are rejected without billing
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI Images upstream exceeds the response limit by one byte
    When the service creates a metered OpenAI Images route and key
    And the client creates an OpenAI-compatible image
    Then the response status is 502
    And the oversized image is unbilled and has no partial response archive

  Scenario: OpenAI image provider errors never expose or archive the upstream body
    Given a token center backed by SQLite and memory object storage
    And the mock OpenAI Images upstream rejects with a sensitive error body
    When the service creates a metered OpenAI Images route and key
    And the client creates an OpenAI-compatible image
    Then the response status is 502
    And the upstream image rejection is sanitized archived as a gap and replayed safely

  Scenario: Codex Responses image tools are exposed as the OpenAI Images API
    Given a token center backed by SQLite and memory object storage
    And the mock Codex Responses upstream returns a generated icon
    When the service creates a metered Codex Responses image route and key
    And the client creates a Codex-backed OpenAI-compatible image
    Then the response status is 200
    And the Codex-backed image response is archived and costs 0.4

  Scenario: Invalid Codex Responses image payloads never expose or archive provider details
    Given a token center backed by SQLite and memory object storage
    And the mock Codex Responses upstream returns a sensitive invalid image payload
    When the service creates a metered Codex Responses image route and key
    And the client creates a Codex-backed OpenAI-compatible image
    Then the response status is 502
    And the invalid Codex image payload is sanitized and never archived
