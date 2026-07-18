# Route-preview coverage corpus

`tokentrimmer.route-preview-coverage.v1.corpus.json` is the versioned,
machine-readable coverage manifest for historical route previews. It classifies
every canonical `tokentrimmer.route.v1` `RouteConditions` field according to
whether a consumer can safely apply it to retained historical request-log data.
It is a compatibility corpus, not a route simulator, a database schema, or an
execution/readiness claim.

The public routing crate verifies that the authoritative copy has exactly one
entry for every canonical `RouteConditions` field. Hosted consumers should
vendor the exact bytes paired with their pinned public revision, reject an
unknown corpus or route-contract version, and use the stable `reason_id` rather
than comparing explanatory prose.

## Format v1

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
