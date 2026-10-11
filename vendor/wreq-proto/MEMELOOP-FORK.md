# wreq-proto 0.2.6: optional local H2 attempt binding

Official archive: https://static.crates.io/crates/wreq-proto/wreq-proto-0.2.6.crate
Archive SHA-256 (original Cargo.lock registry checksum):
`cb4d829f0427a5064829976fe790641ca21abb1d83d844812baf022dc0bdf2db`.
All archive files, including original manifests, VCS receipt and Apache-2.0
LICENSE, are retained. UPSTREAM-SHA256.json inventories authenticated files.
MEMELOOP-SOURCE.patch is the complete, reversible local source/manifest delta.
The ops contract authenticates the official archive and reverses the patch;
unlisted changes, removed upstream files and added runtime files fail closed.

Only this dependency is patched. Versions, wreq, http2, codec, pool behavior,
HTTP policy, timeouts, cancellation and concurrency are unchanged.
`h2-attempt-observer` is enabled only by the root dev-dependency: production
does not compile lifecycle hooks or attach observers. No production switch
is introduced. TEST-only attempt evidence carries the typed request extension.

The numeric token identifies one successful local H2 handshake, not a cached
client or proxy endpoint. Stream IDs come from the actual SendStream. Ordinals
include requests without observers. No transport handle is retained by metadata.
Lifecycle codes: 0 running; 1 connection task returned Ok; 2 returned Err;
3 connection task was dropped without a terminal result. None implies TCP close
or peer acknowledgement; terminal_age_ms is elapsed from local handshake.
Snapshot failures return unavailable, not fabricated identifiers.

No DATA counters, payload/headers, tracing features or wire-delivery assertions
are added. peer_delivery_state remains unknown. Controlled TLS/SOCKS results
cannot identify a production root cause. Any production activation requires
separate approval after actual TEST CI and the bounded source reviews.
