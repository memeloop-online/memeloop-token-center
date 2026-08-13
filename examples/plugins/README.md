# Installable examples

Each first-level directory is a complete read-only plugin package. Point
`MTC_PLUGIN_DIR` at this directory to discover every example, or mount an
individual package directory when testing a ConfigMap-style root manifest.

- `policy-rewrite`: executable traffic policy and request rewrite, plus a
  declarative provider that treats API credentials and OAuth credentials as
  equal connection methods and contributes an OAuth adapter.

These examples contain no production secret or live endpoint. The example
OAuth origin uses the reserved `example.com` domain.
