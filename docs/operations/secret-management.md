# Secret management

## Rules

- Put secret values only in the approved secret manager or Kubernetes Secret
  controller. Never commit them, add them to Helm values, pass them on a command
  line or include them in incident notes.
- Reference individual keys from managed Secrets. Do not overwrite a shared Secret
  as a shortcut because it can revoke unrelated database, archive and bootstrap
  access at once.
- Record only secret names, generation, owner and timestamps in rotation records.

## Rotation

| Secret | Rotation contract |
| --- | --- |
| PostgreSQL password or URL | Create a new database credential, update the managed Secret, roll safely, verify connections, then revoke the prior credential. |
| S3 access key | Grant a second least-privilege key, roll and verify archive read/write, then revoke the first key. |
| Bootstrap service token | Issue a replacement service credential, update trusted callers and the Secret, then revoke the prior generation. |
| Webhook HMAC | Use a controlled delivery interval, update the Secret, roll control pods, switch sender and replay queued events with original idempotency keys. |
| Key pepper | Do not rotate in place. First implement versioned peppers or dual-read/single-write behavior, inventory every credential generation, then retire the prior pepper. |
| TLS private key | Let the approved certificate controller rotate it and verify consumers reload it. |

## Controlled verification

Use short-lived, controller-managed Secret references for a release verification.
Keep both backing credentials valid during the observation period. Verify database
migration/read/write, S3 list/put/get/delete/multipart, one client-authenticated
gateway request, accounting, readiness and rollback. Revoke one old credential at
a time only after database and object-store audit logs show the new principal.

The S3 policy grants only bucket location, list, object read/write/delete and
multipart operations for the exact configured bucket prefix. Do not grant `s3:*`,
administrative object-store permissions or access to unrelated buckets.

## Kubernetes access

- Application service accounts do not need Kubernetes API access and use
  `automountServiceAccountToken=false`.
- Restrict Secret reads to the secret controller and named operators; pods consume
  referenced keys through environment variables.
- Separate production, development and temporary verification principals.
- A Secret update alone does not rotate a running process. Trigger and observe a
  safe rollout, or use an approved reloader.
- Treat checksum annotations as sensitive metadata; do not hash secret values into
  a public manifest.
