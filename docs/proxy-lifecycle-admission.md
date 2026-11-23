Proxy lifecycle admission uses the shared weighted byte budget. A long stream
does not consume a fixed process-wide request slot, so a sixty-fifth request is
admitted when its memory reservations and credential policy allow it.

The obsolete `MTC_PROXY_LIFECYCLE_CONCURRENCY` environment variable is no longer
read or emitted by the chart. Remove `config.proxyLifecycleConcurrency` from
deployment values before upgrading the chart: its strict schema rejects the
removed setting. Lifecycle admission already uses bytes, so removing this
unused setting does not change request admission.

The active lifecycle metric counts actual owners, including streaming handoff
and cancellation settlement. It returns to zero only after those owners release
their activity guards.

Request byte, JSON, SSE, retained-response and disk budgets remain enforced.
Credential concurrency, authorization, usage reservation and settlement remain
unchanged. This change introduces no upload deadline.

Regression coverage holds 65 small silent streams simultaneously, checks the
exported active metric, cancels one and completes the others, and verifies one
ledger entry and one terminal event per request. A separate regression exhausts
the real byte budget and verifies that requests are still rejected. Existing
pre-header cancellation and streaming handoff tests retain their settlement and
owner-release assertions. Validation runs in GitHub Actions only.


The HTTP server also enforces three independent transport limits:

| Environment variable | Helm setting under `config` | Default | Scope |
| --- | --- | ---: | --- |
| `MTC_MAX_TCP_CONNECTIONS` | `maxTcpConnections` | 2048 | TCP connections per server process |
| `MTC_MAX_TCP_CONNECTIONS_PER_IP` | `maxTcpConnectionsPerIp` | 512 | TCP connections per socket peer IP in each process |
| `MTC_MAX_HTTP2_CONCURRENT_STREAMS` | `maxHttp2ConcurrentStreams` | 256 | Concurrent HTTP/2 streams per TCP connection |

Unset values use the defaults. Set positive integers; TCP values must fit the
process semaphore and HTTP/2 accepts up to 4,294,967,295. An invalid value stops
the HTTP server at startup and names the offending setting in the startup log.

The peer IP comes from the accepted socket. A reverse proxy can therefore
aggregate many clients under one peer IP. The per-process TCP limit delays
accepting another socket when all permits are in use; the per-peer limit closes
newly accepted sockets from that peer. The HTTP/2 limit is advertised to the
peer. These counts do not set a request rate or the total number of active
requests, and increasing them does not itself establish capacity. Measure TCP
connections, concurrent requests, throughput, and resource use separately.
