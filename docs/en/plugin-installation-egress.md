# Installation-only egress proxy

The trusted installation policy JSON may set optional `egress_proxy` to an
operator-approved private IP-literal SOCKS5H URL with an explicit nonzero port.
For example, `"egress_proxy": "socks5h://100.64.0.2:1080"` describes a proxy;
this example does not authorize or apply a cluster policy change.

The field is not accepted in installation HTTP requests and cannot come from
an account credential, OAuth token, manifest or inherited environment. It
rejects userinfo, public/loopback/link-local/metadata addresses, DNS hostnames,
non-SOCKS5H schemes, paths, queries and fragments. Private-address validation
reuses the host's existing network policy helper. The mounted policy remains
limited to 64 KiB and rejects unknown fields. Omitted/null preserves legacy
configuration and trust fingerprints; a configured proxy is included in the
review/checkpoint trust fingerprint, so changes require fresh review.

The host passes `--egress-proxy` to its co-located installer. Standalone
and descriptor-import installation accept the same flag. OCI manifest,
configuration and layer fetches use an explicit proxy client with remote DNS.
Client construction errors fail closed rather than falling back to the
library's default client. Production targets remain HTTPS and exact source
allowlisting, digest verification and all package validation remain unchanged.

The installer supplies only explicit `HTTP_PROXY`, `HTTPS_PROXY` and empty
`NO_PROXY` to its Cosign process, alongside the existing private workspace
variables. Cosign's environment is still cleared; no service-Pod global proxy,
arbitrary environment inheritance, account token or TLS/signature bypass is
introduced. Go's HTTPS proxy transport supports SOCKS5H with remote DNS. Exact
signer, issuer, version, certificate, claims, SCT and Rekor checks remain on.
Registry credentials retain the existing separately reviewed file references.

## Authenticated SOCKS

The policy also accepts optional `egress_proxy_username_file` and
`egress_proxy_password_file`. Both must be absolute mounted-file references,
provided together with `egress_proxy`; the endpoint still rejects userinfo.
The corresponding installer flags have the same names with hyphens. HTTP
installation requests reject all three fields. There are no proxy credential
value flags, inherited credential variables, or account-proxy fallback.

Reuse the existing Control-only Secret projection:
`plugins.runtimeInventory.registrySecrets` selects existing Secret names/keys
and mounts them read-only at `/var/run/mtc-plugin-registry/<index>/<key>` with
mode `0440`. Despite the historical mount name, selecting dedicated proxy keys
does not configure registry authentication. Only `source_credentials` controls
registry authentication. Do not reference account transport groups or OAuth
credentials; those have a different owner and trust boundary. No chart schema,
identity, shared proxy configuration, or anonymous/IP exemption is required.
If the existing protected proxy configuration bundles authentication in a YAML
key, that entire configuration is not a username/password file: its owner must
provide protected projections of the same approved credentials, without
printing/extracting them into tickets or changing the proxy's authentication.

The child installer reads each regular secret file with a strict bounded read.
UTF-8 values must contain 1–255 bytes after trailing CR/LF removal; embedded
control characters are rejected. Values are percent-encoded only in RAM for
the OCI client and the cleared Cosign child environment. They never enter
argv, API responses, receipts, diagnostics or Docker registry configuration.
The credential type has redacted Debug output, and read/parse failures return
only the existing fixed egress-policy error. The proxy must remain on an
approved private network: SOCKS username/password negotiation itself is not
encrypted. Secret mounts and process-environment access retain the existing
Control runtime security boundary.

Credential file references join the review/checkpoint fingerprint. As with
registry credentials, their contents are transport authorization, not signing
authority, and are not embedded in that fingerprint. Rotating contents at the
same references does not weaken or replace fresh package signature checks.
Omitted/null credential fields retain anonymous behavior and old fingerprints.

Failure diagnostics preserve fixed workspace/version/verify phase, numeric
exit code/errno, and allowlisted stderr-pattern classes. They never retain or
emit raw subprocess stderr, stdout, URLs, paths, credentials or user payload.
The host validates the envelope and allowlists fields again. A pattern class
is a diagnostic hint, not proof of network behavior or installation success;
the API still exposes its existing safe failure category.

Existing GHA native tests cover proxy URL rejection, policy-to-CLI binding,
trust fingerprint changes, Cosign environment isolation, OCI SOCKS remote-DNS
transport, authenticated RFC1929 negotiation (including encoded punctuation),
rejected authentication without direct fallback, secret-file bounds, registry
auth separation and hostile diagnostic fields. The SOCKS relay tests exercise
the transport builder with a loopback fixture; production validation continues
to reject loopback proxies. No new workflow, dependency or build pipeline is
required. This source change alone does not deploy a proxy policy, install or
activate a candidate, or establish OAuth/login readiness.
