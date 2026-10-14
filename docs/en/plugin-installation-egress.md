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

The host passes only `--egress-proxy` to its co-located installer. Standalone
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

Failure diagnostics preserve fixed workspace/version/verify phase, numeric
exit code/errno, and allowlisted stderr-pattern classes. They never retain or
emit raw subprocess stderr, stdout, URLs, paths, credentials or user payload.
The host validates the envelope and allowlists fields again. A pattern class
is a diagnostic hint, not proof of network behavior or installation success;
the API still exposes its existing safe failure category.

Existing GHA native tests cover proxy URL rejection, policy-to-CLI binding,
trust fingerprint changes, Cosign environment isolation, OCI SOCKS remote-DNS
transport and hostile diagnostic fields. The local SOCKS relay test exercises
the transport builder with a loopback fixture; production validation continues
to reject loopback proxies. No new workflow, dependency or build pipeline is
required. This source change alone does not deploy a proxy policy, install or
activate a candidate, or establish OAuth/login readiness.
