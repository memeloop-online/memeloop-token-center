# Overview and usage presentation

Statistics keep their API values. Compact counts, human-readable durations and
two-decimal currency labels expose the original precision in a tooltip. Unlike
currencies remain separate.

Metric backgrounds use the returned time series, or an actual success ratio.
They do not synthesize trends for missing, single-point or all-zero series.
Overview shares its existing independent trends request with the snapshot cards;
failed trends do not prevent the monitoring snapshot or recent requests loading.

P95 is a fixed-histogram estimate, not an exact percentile. Finite buckets display
their upper bound. `p95_is_capped=true` identifies the open-ended >60-second bucket;
the UI displays the lower threshold and omits this point from latency plots.
Older payloads without the flag cannot distinguish the highest finite bucket
from overflow at 60,000 ms, so show an unknown highest-bucket label and plot a gap.
The flag is derived from existing buckets and requires no database migration.

Top upstream entries are the server's top ten account/model pairs by terminal
request count in the selected window. Identity is account UUID plus model, not
display name. Model headings are presentation groups, not aggregate model ranks;
percentiles and currencies are never combined client-side. Accounts without
terminal traffic do not acquire fabricated rows. Account UUIDs remain available
as technical tooltips instead of primary labels.
