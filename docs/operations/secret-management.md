# Secret management

The Helm chart references existing Kubernetes Secrets and never templates secret
values. Production should use a secret controller backed by a dedicated secret
manager, or encrypted GitOps such as SOPS with a cluster-side decryptor. Plain
Secret manifests and shell history are not acceptable secret stores.

## Required keys

The default Secret references contain:

- `database-url`;
- `key-pepper`;
- `service-token` bootstrap credential, generated from at least 32 random bytes
  and encoded without Unicode whitespace or `Cc` control characters;
- `s3-access-key` and `s3-secret-key`.

Upstream credentials and issued review credentials are separate operational
secrets and must have an owner, purpose and expiry.

The optional MemeLoop Cloud integration uses a separate
`memeloop-cloud-webhook-secret` HMAC key. Do not reuse the bootstrap service
token or key pepper. Leaving its Secret reference name empty keeps the signed
subscription endpoint fail-closed.

The application enforces the `service-token` format at startup: its encoded
value must be at least 32 bytes and must not contain Unicode whitespace or
Unicode General Category `Cc` control characters anywhere. Rejecting them
avoids ambiguity from Secret-file line endings and invisible characters in a
copied bearer credential. Generate the token in the secret manager when
possible; `openssl rand -hex 32` is an offline equivalent that generates 32
random bytes and encodes them as 64 printable ASCII characters. This check is a
minimum format rule and does not measure the value's entropy.

## Avoid last-applied disclosure

Do not create Secrets with `kubectl apply` in a way that writes the complete
manifest into `kubectl.kubernetes.io/last-applied-configuration`. Base64 is an
encoding, not encryption; the annotation duplicates every secret value and is
readable by anyone who can read Secret metadata and data.

Prefer server-side apply from the secret controller. Audit existing Secrets for
the annotation without printing it, remove it through the authorized GitOps or
secret-management workflow, and rotate any independently rotatable values that
may have been exposed. Never include decoded or encoded values in tickets, logs,
CI output or screenshots.

Removing the annotation only removes the duplicate; it does not make a value
that was previously readable secret again. Treat every independently rotatable
value in an affected Secret as exposed until its replacement has completed.
Do not delete or overwrite the shared Secret as a shortcut: that can revoke the
database, archive and bootstrap credentials at the same time.

## Rotation

| Secret | Rotation contract |
| --- | --- |
| PostgreSQL password/URL | Create the new database credential, update the managed Secret, roll pods, verify all connections use it, then revoke the old credential. |
| S3 access key | Grant a second least-privilege key, roll and verify archive reads/writes, then revoke the first key. |
| Bootstrap service token | Issue a replacement service credential, update trusted callers and the Secret, then revoke the old generation. |
| MemeLoop Cloud webhook HMAC | The runtime accepts one generation, so use a controlled integration window: pause deliveries, update the managed Secret and roll control pods, switch the sender, then replay queued events with their original idempotency keys and fresh timestamps. |
| Key pepper | **Do not rotate in place.** Existing downstream and migrated CPA credentials depend on it. First implement versioned peppers or dual-read/single-write migration, inventory every credential generation, and only then retire the old pepper. |
| TLS private key | Let cert-manager or the approved controller rotate it; verify consumers reload it and old keys are retired. |

## Dogfood canary with dual credentials

Do not mutate the live combined Secret for the first canary. The chart accepts
independent Secret references, so create short-lived, controller-managed
Secrets for each rotatable boundary and keep both old and new backing
credentials valid during verification:

```yaml
config:
  databaseUrlSecret:
    name: memeloop-token-center-database-canary
    key: database-url
  # Deliberately keep the existing pepper reference. Do not copy, regenerate or
  # rotate this value during the canary or CPA credential history will break.
  keyPepperSecret:
    name: memeloop-token-center-secrets
    key: key-pepper
  serviceTokenSecret:
    name: memeloop-token-center-service-canary
    key: service-token
  memeloopCloudWebhookSecret:
    name: memeloop-token-center-cloud-canary
    key: memeloop-cloud-webhook-secret
  s3:
    credentialsSecret:
      name: memeloop-token-center-archive-canary
      accessKey: s3-access-key
      secretKey: s3-secret-key
```

Use this order:

1. Create a second PostgreSQL principal with only the required database/schema
   privileges. Keep the old principal valid. Put only its URL in the database
   canary Secret.
2. Create a dedicated S3 principal scoped to the exact archive bucket. It needs
   bucket list/location and object get, put, delete, copy and multipart
   operations because readiness, streaming finalization and retention use all
   of them. It must not be the MinIO root credential. Put it in the archive
   canary Secret while the old principal remains valid.
3. Issue a new short-lived canary service credential. Keep external production
   callers on the old credential and route only canary checks to the new
   release. Never place either value in Helm values or a command argument.
4. Deploy a separate canary release/route with the Secret references above and
   explicit NetworkPolicy rules. Verify database migration/read/write, S3
   list/put/get/delete/multipart, one credential-authenticated proxy request,
   accounting, `/readyz`, and a rollback to the old release.
5. Promote the new references to the dogfood release, migrate callers, observe
   at least one complete credential TTL/connection lifetime, then revoke the old
   database, S3 and service credentials one at a time.
6. Delete the canary route and short-lived Secrets through their controller.
   Confirm the resulting Secrets have no last-applied annotation and record
   only names, generations, owners and timestamps in the rotation log.

Rollback before revocation means routing back to the old release; both backing
credentials are still valid. After promotion, revoke only after the new
principal is observed in database/S3 audit logs and the old one is quiescent.
The key pepper is intentionally outside this dual-credential procedure.

A MinIO-compatible least-privilege policy has this shape (replace the bucket
name before installation):

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "s3:GetBucketLocation",
        "s3:ListBucket",
        "s3:ListBucketMultipartUploads"
      ],
      "Resource": ["arn:aws:s3:::memeloop-token-center-dogfood"]
    },
    {
      "Effect": "Allow",
      "Action": [
        "s3:GetObject",
        "s3:PutObject",
        "s3:DeleteObject",
        "s3:AbortMultipartUpload",
        "s3:ListMultipartUploadParts"
      ],
      "Resource": ["arn:aws:s3:::memeloop-token-center-dogfood/*"]
    }
  ]
}
```

Do not grant `s3:*`, `admin:*`, access to other buckets, or MinIO root/admin
credentials. S3 server-side copy is authorized by source `GetObject` plus
destination `PutObject`; it does not need an account-wide permission.

The application reads environment-backed Secrets at process start, so a Secret
update alone does not rotate running pods. The release workflow must trigger and
observe a safe rollout, or use an approved reloader. Avoid hashing secret values
into a public manifest; if checksum annotations are used, the checksum itself
must be treated as sensitive metadata.

## Access control

- Application service accounts do not need Kubernetes API access and keep
  `automountServiceAccountToken=false`.
- Restrict Secret read permissions to the secret controller and named operators;
  pods consume only referenced keys through environment variables.
- Separate production, dogfood and development Secrets and database principals.
- Record issuance, rotation and revocation without recording the secret value.
- Periodically remove expired review/test credentials and verify no credential is
  shared between the old CPA service and the new service unless deliberate for
  a documented migration window.
