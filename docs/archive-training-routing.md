Reader-first training CAS rollout

Legacy unqualified locators retain their original MTC_S3_* backend. Independent
MTC_TRAINING_S3_ENDPOINT/BUCKET/REGION/ACCESS_KEY/SECRET_KEY/ALLOW_HTTP configure
training reads; endpoint presence requires the other mandatory fields. Secrets
must remain Kubernetes Secret references. MTC_TRAINING_CAS_WRITES defaults false.

Qualified locator:
  stores/training/<backend identity>/tenants/<tenant>/cas/v1/blake3/<shard>/<digest>
The identity is BLAKE3 of serde_json's UTF-8 tuple encoding:
  ["mtc-training-s3-v1", endpoint, bucket, region]
It binds the exact endpoint string, bucket and region. Changing that binding
cannot reinterpret historical references: reads fail identity comparison. A
credential rotation does not change identity. Preserve the original endpoint
alias and bucket, or implement a separately reviewed identity mapping first.

Reader rollout must reach every reader role before enabling any qualified write.
No application deployment is performed by this PR, and the user's production
rollback must be preserved. Once network access and compatible readers are proven,
MTC_TRAINING_CAS_WRITES=true routes new put_tenant_content objects (including
conversation event/prefix CAS) to training. Every returned locator is qualified;
CAS creation and streamed readback verification precede returning it. Bare-key
reads never probe training, and missing training reads never fall back to legacy.
Raw/staging writers, durable roots, terminal receipts and typed staging cleanup
remain on legacy. In particular, completing old staging promotes within legacy
regardless of the new-CAS write switch. Raw-cache duplicate keys retain training
identity when applicable; existing five-component keys retain legacy meaning.

This is not a complete new-write cutover. Streaming request/body staging still
needs durable backend identity in its staging handle and relational lease before
it can safely change backend. No pending row, old object, or receipt is deleted.
There is no archive gate added to model requests.

Online historical reference switching remains a separate migration-tool change:
use only fields with an existing reader-supported reference representation;
read back target CAS plus its immutable receipt, bind exact training identity,
and recheck tenant/primary key/source digest and length under a short source-row
lock before conditional update. Changed/disappeared rows remain unchanged and
are counted as skipped. Do not rewrite outbox request_json/replay-digest fields.
Existing live copy receipts keep their identity and keys; qualification changes
only the relational locator after supported-reader verification. Copy completion
does not reclaim PG storage or authorize source deletion.
