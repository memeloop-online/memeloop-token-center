# Bounded peer-stream diagnostic support

This is deployment diagnostic support, not MTC request-path code, a migration
utility, or a capture service. It has no production installation or execution
workflow. The separate `peer-stream-fingerprint` GitHub Actions workflow uses
stdlib synthetic fixtures and an isolated hosted-runner container only. Do not
run these tests or install dependencies in the shared local workspace.

## Evidence contract

`peer_fingerprint.py` reads classic PCAP from the fixed tcpdump child's anonymous
pipe. Ethernet, Linux cooked v1/v2, raw IPv4 and IPv4 link types are supported;
fragments, truncation, other peer/port traffic and ambiguous oversized frames are
rejected. No raw packet bytes, payload text, tcpdump stderr, credentials, TLS keys,
pcap files or core dumps are persisted. Stdout contains bounded JSON only.

Each direction is reassembled by TCP sequence, not packet boundaries. Identical
retransmits and resegmentation do not create differences. SYN consumes sequence
space; 32-bit wrap is unwrapped within a bounded 16MiB span. Full 4096-byte aligned
intervals are hashed only after coverage is complete. The comparison requires
the same flow, generation, absolute wire offset and length. A bounded span avoids
multiple wraps. Late conflicting retransmits invalidate the entire flow; retained
RAM data is not evicted to manufacture a successful comparison.

Existing flows are normally midstream: their TCP generation is not proved merely
by matching a four-tuple. Tuple reuse, missing sequences, incomplete coverage,
capture loss, mismatched windows, OOM, an abnormal exit or a budget stop cannot
certify transport integrity. Matching records must lie entirely in the common
observed time window. Summary comparison **always returns `inconclusive` and
`attribution: not_established`**. It reports only counts of equal/unequal matching
intervals; same-window bad-MAC reproduction, clock alignment, connection identity
and local offload effects require separate read-only review. Neither no mismatch
nor no reproduction is PASS; even a synthetic mismatch is not causal attribution.

## Bounded implementation

- At most 32 directional flows and 1536 blocks: 6MiB payload plus coverage and
  bounded metadata in RAM. Budget exhaustion stops capture, without eviction.
- Each frame is at most 256KiB. Tcpdump buffer is 512KiB. Each endpoint emits one
  summary of at most 3MiB; two endpoints plus a small comparison/receipt fit 8MiB.
  Retain only one durable copy per endpoint; container logs are also summaries.
- An independent forked watchdog interrupts the capture process group at the
  deadline, then kills it after at most two seconds. Parent death closes a control
  pipe and triggers group cleanup. Normal/error paths reap children. A separate
  Pod deadline is still required to bound the parser/container itself.
- `PYTHONDONTWRITEBYTECODE=1`, Python `-B`, a read-only root filesystem and disabled
  core dumps prevent bytecode/core persistence. The helper never writes a file.

## CI scope and actual measurements

The workflow grants only `contents: read`, disables checkout credential
persistence and has no cluster credentials, package publishing or image build.
It tests segmentation, retransmission, reordering, gaps, conflicts, wrap, SYN/FIN,
connection reuse, malformed PCAP, scope rejection, comparison quality gates,
budgets, parent death, parser failure/stall and child-group cleanup.
The same unit suite also runs in the pinned tool image; that test runner is
separate from the strictly limited capture resource measurement. Checkout and
the resource receipt identify the exact PR head, not only its synthetic merge.

`ci_resources.py` anonymously pulls the already inventoried immutable image:

```
docker.io/nicolaka/netshoot@sha256:b09d9b21381f47a79b3cbcb30da25266dc17186ea00ae65e99fdc51396f48e70
linux/amd64 manifest: sha256:147f2c4cb0aa7a741151bce2bfef71d9e1a04631636e24665ee2a5ed799feb47
config: sha256:2298d942ce5f8e8083c036b6d62e129379998712ef78343f88d41d782d507141
```

It checks the resolved config/architecture, then runs `/usr/bin/python3` with
`/usr/bin/tcpdump` under **one total 32MiB / 200m cgroup**, proposed UID/GID 0,
NET_RAW only, no-new-privileges, no writable root, no network, no swap and a 12-PID
limit. The read-only support source is the only CI bind mount. No production
hostPath is proposed. Tcpdump observes an idle isolated loopback interface with
the documentation-address pair filter; no network traffic is generated. While
the real capture child and watchdog live, Python fills the entire assembly budget
from synthetic bytes and serializes a maximum-sized summary into a counting sink.
The report records whole-cgroup `memory.peak`, OOM/exit status, timing and cleanup,
not a misleading sum of process RSS. It does not measure production throughput.

**Identity change requires parent review before production:** the initial
nonroot Docker smoke failed, not from OOM. Run 37462208864, head `8b31bb64`,
measured CapBnd=0x2000 but CapEff=CapPrm=CapAmb=0 and tcpdump permission rejection
at both 32MiB and 64MiB. The revised CI tests UID 0, still nonprivileged with only
NET_RAW, rather than adding SETUID/SETGID/NET_ADMIN, changing file capabilities,
installing a launcher or changing node runtime configuration. The helper checks
effective/permitted/bounding capabilities are exactly NET_RAW and requires
NoNewPrivs=1. Do not add `-Z root`: the CMake build defaults to no `WITH_USER`,
so that option requests an unnecessary uid/group transition requiring additional
capabilities. The conditional behavior is explicit in the publisher's
[tcpdump 4.99.6 source](https://github.com/the-tcpdump-group/tcpdump/blob/tcpdump-4.99.6/tcpdump.c)
and [CMake configuration](https://github.com/the-tcpdump-group/tcpdump/blob/tcpdump-4.99.6/CMakeLists.txt).
Green CI for this alternative is not approval to change the production identity.

If 32MiB fails, a **CI-only 64MiB diagnostic measurement** is reported, but the
required check remains failed even if that measurement succeeds. No production
budget silently changes. Docker cleanup and a 90-second harness deadline bound
each measurement. Resource evidence is a metadata-only seven-day GHA artifact.

## Production gate: not executed by this PR

Actual capture needs exact-head green checks, review and the parent's explicit
one-window confirmation. No capture Pod, ConfigMap or host change is created here.
The proposed design remains two temporary hostNetwork Pods, hostPID/hostIPC false,
NET_RAW only, nonprivileged, UID 0 subject to explicit identity review as above,
no SA token/Secret/hostPath/application PID,
32MiB/200m **total per Pod**, restartPolicy Never, activeDeadlineSeconds 300,
terminationGracePeriodSeconds 2. Mount only the reviewed non-secret helper/settings
read-only. Check exact helper SHA256, image identity and both-end readiness first;
failure does not authorize an automatic retry or a larger memory budget.
An OOM may kill the watchdog too: the independent Pod/container deadline is
mandatory, not replaced by the Python watchdog or a claim that it survives OOM.

The production CLI fixes `/usr/bin/tcpdump`, `tailscale0`, `-p` (no promiscuity),
`-nn`, port 2380 and a bidirectional filter for the two explicit IPv4 peers. There
is no arbitrary capture expression or command option. Use the same 32-hex window
ID and future `--start-unix-ns` on both ends, `--endpoint left/right`, and at most
`--duration 240`. Missing the common start fails closed. No live parser attribution
is allowed until synthetic CI has passed; no auth or probe traffic is generated.

Archive only bounded summaries and their `compare` result. OOM, drops, limits or
failure to reproduce mean inconclusive, not permission for another window. Delete
only the exact temporary capture resources after the approved window. Never modify
MTU, offload, firewall, routes, membership, certificates, etcd, PG or volumes here.
