# Client credentials

A client credential represents an application or automated task. It determines which models the client can call and limits `/self/v1/*` views to that credential's requests, statistics, and sessions.

## Use a credential

Send the credential in the `Authorization` header over HTTPS:

```http
Authorization: Bearer mtc_example_client_key
```

Client credentials are for client interfaces and do not replace other administrative identities. The available models come from the routes and permissions configured for the deployment.

## Store and rotate safely

- Keep credentials in a server-side secret store or a controlled local development configuration.
- Do not put credentials in browser code, logs, screenshots, commit history, or error messages.
- If a credential is exposed, rotate it through the credential-management surface provided by the deployment and update every client that uses it.
- Use separate credentials for separate applications so access can be revoked and audited independently.

## Self-service views

A client can use the same credential to query its own data:

```bash
curl "https://mtc.example.com/self/v1/stats" \
  -H "Authorization: Bearer mtc_example_client_key"
```

Common self-service paths include:

- `/self/v1/requests`: requests for the current credential
- `/self/v1/stats`: usage and cost statistics
- `/self/v1/sessions`: session views
- `/self/v1/conversations`: replayable conversation records

Responses contain only data visible to the current credential. Deployment administrators issue, authorize, and revoke credentials; clients use the models and paths they have been granted.
