# Latency-routing evidence and operation boundaries

Scope: live chat selection's `upstream_latency_ms_p95_gt` condition and the in-process `tt_routing::latency::LatencyTracker`. This is not a provider/fleet SLO or a complete attempt ledger.

## Which signal a request uses

| Incoming request | History consulted | What the measurement means |
| --- | --- | --- |
| `stream: true` | `StreamEstablishment` | Elapsed time until the provider adapter returns a stream handle, including configured retries/backoff |
| Buffered / `stream: false` or omitted | `BufferedCompletion` | Elapsed time until a full buffered provider result is returned, including configured retries/backoff |

A buffered request no longer borrows stream-establishment history. If the matching operation is cold, stale or unavailable, the condition does not match; it does not substitute the other population. The threshold remains strictly greater-than.

**Rollout changes can affect matching.** The prior consumer always consulted stream-establishment samples even for buffered requests. Buffered routing could remain inert despite slow buffered history, or reroute based on unrelated stream history. Review rules and thresholds for both request modes before adopting this change; a threshold appropriate for establishing a stream may be too low for generating a full response.

Stream-handle readiness is **not first output token**, TCP connection latency or streaming completion time. Adapters can differ in when they expose a handle. First-token and complete streaming-duration objectives need their own measured contracts; they are not supplied by this change.

## Freshness and scope

`LatencyTracker::evidence(provider, model, operation)` returns one read-locked snapshot containing:

- `scope: instance` and the selected operation;
- the 300-second maximum age, 256-sample per-key capacity and 20-sample minimum;
- retained and fresh sample counts;
- newest/oldest **fresh** sample ages in monotonic milliseconds;
- nearest-rank p95, or unknown below the minimum.

Samples at the exact age limit are excluded. No fresh samples means unknown p95 and absent fresh ages, not zero latency or a freshly updated observation. A missing key is a known empty window; a poisoned lock is unavailable (`None`). The legacy count helper still maps unavailable to zero for compatibility; use the evidence snapshot when that distinction matters.

Live chat selection consumes that same snapshot for its decision and DEBUG diagnostic. This adds no prompt values or new public HTTP endpoint and does not persist a customer-facing request explanation. R02's broader explanation/retention contract remains separate.

The key is provider/model/operation, **not** tenant, workload, credential, request size or region. AppState clones share their tracker; independent trackers/processes do not share evidence. This is not a cross-replica aggregate. Each ring is bounded, but the total keyspace is not capped and expired keys are not reclaimed here.

## Population limitations

Current dispatch wiring records successful direct dispatch groups. It includes retries/backoff rather than individual attempt times. A streaming establishment can be recorded even if the stream later fails. The configured fallback-chain branch does not currently feed this tracker, including when its first candidate succeeds. Cache hits, failed/deadline-aborted groups, shadow calls and other endpoint/run paths are not comprehensive observations here.

Consequently, timeout rates and full attempt/failure denominators are **unmeasured**, not zero. A success-conditional p95 can look healthy during failures and heterogeneous request sizes can change it. Use separate reliability/budget controls; do not promote this signal into a latency guarantee or customer task-quality claim.

## Local acceptance

```sh
cargo test -p tt-routing --lib --locked --offline
cargo test -p tt-core --test route_latency_operations --locked --offline
cargo clippy -p tt-routing --all-targets --locked --offline -- -D warnings
cargo clippy -p tt-core --lib --test route_latency_operations --locked --offline -- -D warnings
```

The HTTP fixtures use authenticated keys and recording providers with seeded histories, not paid inference or real latency measurements. Deterministic window tests cover expiration, exact percentile/count/age agreement, operation separation, capacity and poisoned-lock refusal. Hosted/fleet acceptance, first-token instrumentation, timeout/attempt coverage, preview/operation breadth and workload normalization remain open S09/R02/A05 work.
