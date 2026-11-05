SQLx postgres 0.9.0, copied from crates.io. MTC exposes the server-issued
cancellation key through the Any backend and adds CancelRequest transport using
the existing PgStream connection options, including configured TLS and Unix
socket handling. Cancellation keys are sensitive. Remove this patch with the
companion sqlx-core patch when upstream supports the same API.
