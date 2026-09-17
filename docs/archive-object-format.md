# Versioned text archive objects

This format changes storage, not the request/response byte contract. It is
independent of encrypted database-spool compression and media-retention policy.

## Deployment

Deploy the compatible reader to every gateway/control/worker first, leaving
`config.archiveObjectCompression.enabled=false` (environment:
`MTC_ARCHIVE_OBJECT_COMPRESSION_ENABLED`). Then enable new writes. Disabling the
flag freezes new compressed writes; it does not remove reader compatibility.
Once compressed objects exist, rolling readers back to pre-format code is not
safe. No historical object is overwritten, migrated, or deleted by this change.

Only the text proxy spool uploader opts into compression. Media writers and
legacy `put`/`put_content` keep their old behavior; media non-retention is the
separate #277 policy. A failed or cancelled compressed upload uses the existing
owned staging lease/abort/reaper path and cannot bind an unfinished object.

## Wire contract

- Locator suffix `.mtcz1` explicitly selects v1; no magic sniffing of legacy data.
- Header: eight ASCII bytes `MTCZSTD1`.
- Frames: little-endian u32 plaintext length, u32 encoded length, 32-byte BLAKE3
  plaintext digest, then one zstd level-1 frame. The writer coalesces input into
  64-KiB blocks independently of upstream chunk boundaries.
- Footer: eight ASCII bytes `MTCZEND1`, little-endian u64 total plaintext length,
  then 32-byte BLAKE3 digest of the complete original plaintext.
- Decompression is bounded per block (64 KiB), encoded blocks are at most
  128 KiB, and total plaintext is at most the existing 64-MiB spool limit.

The durable locator remains inside the same request/attempt staging prefix.
Compressed writers reject an already-suffixed input and support only
`finish_staged`: shared CAS locators retain their existing exact 64-hex contract.
`StagedArchiveObject.size_bytes` and its digest describe the original bytes.
`head_size`, bounded reads, and byte ranges expose logical plaintext lengths.
Ranges scan and verify the complete compressed object in this first version:
they do not imply cheap random access. The last selected block is held until
footer/EOF verification. Earlier blocks may already have streamed, but an
integrity failure never becomes a clean EOF. Reads pin the footer's object
version/ETag where the backend provides them.

## Separate follow-ups

1. Prefix/content-block storage needs a versioned lossless byte manifest,
   tenant-scoped chunk identity, ordered byte reconstruction, reference-safe
   garbage collection, and old/new dual reads. Existing semantic atoms are
   normalized and cannot reconstruct exact JSON whitespace, SSE framing, or
   opaque fields. They must not replace the raw byte contract implicitly.
2. Text proxy archives apply a typed retention policy before encryption:
   encrypted fields inside explicit reasoning/encrypted protocol envelopes,
   inline image/audio/video data URLs, and typed media body fields become
   bounded metadata markers. Delivery, billing, conversation projection and
   upstream request bytes remain unchanged. Unknown fields and non-JSON bodies
   retain their original bytes; the policy does not guess from entropy or
   redact ordinary tool fields named `data`, `b64_json` or
   `encrypted_content`. Ambiguous duplicate-key requests are rejected before
   dispatch; duplicate-key upstream JSON is retained only as metadata because
   its exact bytes can contain a value hidden by normal JSON object projection.
3. Historical recompression must write a new object, verify original
   length/digest, CAS-bind the new locator, and reclaim the old object only
   through lease/reference-safe GC. It is not part of enabling new writes.
