# Route-preview coverage corpus

`tokentrimmer.route-preview-coverage.v1.corpus.json` and later versions are
the versioned, machine-readable coverage manifests for historical route
previews. Each classifies every canonical `tokentrimmer.route.v1`
`RouteConditions` field according to whether a consumer can safely apply it to
retained historical request-log data. They are compatibility corpora, not route
simulators, database schemas, or execution/readiness claims.

The public routing crate verifies that the authoritative copy has exactly one
entry for every canonical `RouteConditions` field. Hosted consumers should
vendor the exact bytes paired with their pinned public revision, reject an
unknown corpus or route-contract version, and use the stable `reason_id` rather
than comparing explanatory prose.

## Format

```json
{
  "corpus": {
    "id": "tokentrimmer.route-preview-coverage-corpus",
    "version": 1
  },
  "route_contract": {
    "id": "tokentrimmer.route.v1",
    "version": 1
  },
  "conditions": [
    {
      "field": "tag_equals",
      "classification": "exact",
      "reason_id": "tag_retained"
    }
  ]
}
```

`conditions` is in canonical `RouteConditions` declaration order. Every field
appears exactly once. A `reason_id` is a stable, lowercase snake-case identifier
that consumers map to their own bounded explanatory copy.

`classification` has three values:

- `exact`: the historical record retains the same decision input, so the
  condition may be included in the historical predicate.
- `approximate`: a retained value supports a useful historical comparison but
  is not the exact runtime decision input. The condition may be included only
  with its approximation caveat.
- `unavailable`: the required runtime decision input is not retained in a
  compatible historical form. The condition must stay visible to the operator,
  but must not be bound into a historical predicate.

The manifest concerns only active condition evidence. It says nothing about
route priority, target/action behavior, credentials, pricing, provider health,
future request acceptance, or whether a historical result predicts a future
gateway decision.

## v2 requested-model snapshots

v2 changes only `model_in`: gateway rows written after migration 0042 retain
the caller's pre-routing `requested_model`, so v2 classifies it as `exact` with
the stable `requested_model_snapshot_retained` reason ID. The final served
`model` remains unsuitable for this predicate.

This is exact only within rows that carry a non-NULL snapshot. Historical rows,
and rows written by an older gateway during a rolling deploy, remain NULL and
must make a window explicitly partial; consumers must never backfill or infer
the caller model from the served model. Even fully snapshot-covered condition
evidence is still not a replay of route priority, actions, credentials,
pricing, cache/failover, quality, or a future gateway decision.
