# Kimi reviews with a complete plaintext task

Supply the full task and source to the ordinary Chat API. This is the path that
previously completed real Kimi reviews. It uses normal MTC routing, request
archiving, usage and billing, with an authorized downstream API key held in the
process environment. It does not read or copy provider OAuth credentials.

With `MTC_API_KEY` already set, run:

```console
node scripts/maintenance/kimi-review.ts < complete-review-task.txt
```

The default model is `k3.1-pro`. `MTC_REVIEW_MODEL` changes the model;
`MTC_BASE_URL` changes the gateway URL, including its `/v1` prefix. The command
prints the actual review, finish reason, usage and request ID as JSON. Keep the
request ID to inspect the archived plaintext task and returned review. It sends
one request, leaves thinking at the provider default and sets no artificial
output token cap. An empty review or a `length` finish reason remains visible.

This runs a Kimi review; it does not create a Codex child or give the model local
tools. Native Codex reserves the `collaboration` schema. Changing its encrypted
message fields causes a provider 400. Existing opaque client task envelopes
cannot be converted into full plaintext without their owning client's key.
Supply the original task and source here instead of replaying an opaque envelope.
