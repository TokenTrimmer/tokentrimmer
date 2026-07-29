# Gateway capabilities contract corpus

This directory publishes a generated structural schema plus a small versioned
compatibility corpus for the public gateway's authenticated
`GET /v1/capabilities` document. `gateway-capabilities.schema.json` comes from
the exact Rust `GatewayCapabilitiesDocument` response type; its matching
TypeScript is in `bindings/product-contracts.generated.ts`, and the Python SDK
uses the generated frozen dataclasses in
`sdk-python/tokentrimmer/product_contracts_generated.py`.

`tokentrimmer.gateway-capabilities.v1.corpus.json` remains the semantic parser
drift guard. Neither artifact is a second runtime validator or readiness
evidence.

`request-preflight-response.schema.json` separately describes the authenticated
`POST /v1/capabilities/preflight` response. That endpoint performs no provider
I/O: it reports request-specific provider resolution, exact catalog capability
and caller-declared-token comparisons, organization credential-record presence
without decrypting the stored secret, and an explicitly bounded standard-rate
catalog cost projection when priceable, while keeping provider health and
request acceptance unknown. The cost interval is not live provider pricing,
tokenization, readiness, a quote, reservation, settlement, invoice, or
enforced budget.

`request-preflight-batch-response.schema.json` wraps 1–9 of those declarations
and responses in one ordered, single-responder document with one generated-at
marker. It removes cross-process drift for a role set but explicitly does not
make credential/configuration reads transactional or establish provider
readiness, executable panel admission, reservation, or execution.

The public CLI test consumes the authoritative copy. Hosted consumers should
vendor its exact bytes with the public revision they pin, then run their own
parser against every `dashboard` projection.

## Format v1

```json
{
  "corpus": {
    "id": "tokentrimmer.gateway-capabilities-contract-corpus",
    "version": 1
  },
  "contract": {
    "id": "tokentrimmer.gateway-capabilities.v1",
    "version": 1
  },
  "cases": [
    {
      "id": "stable-case-name",
      "document": {},
      "expected": {
        "cli": { "outcome": "accepted", "fusion": {} },
        "dashboard": { "outcome": "accepted", "fusion": {} }
      }
    }
  ]
}
```

Each case has a unique identifier, one complete public wire `document`, and
one expected result for each consumer. Accepted projections include a compact,
message-free canonical Fusion view: switch/access state plus their reason
codes, current/minimum tier facts, and the process-local member cap. Rejected
projections deliberately carry no reason prose because explanatory copy is not
the portable contract.

That Fusion value is a shared, snake-case comparison projection, not either
consumer's serialized object shape. The dashboard corpus test maps both its
server-side gateway parser and browser-side redacted-probe parser into it
before comparing results. The representative enabled, disabled, and
tier-blocked vectors use the current public builder's reason codes; the public
bridge test keeps those emitted semantics aligned while allowing the actual
process-configured member cap to vary.

Accepted vectors model the public v1 wire shape. Rejection vectors deliberately
violate one strict consumer rule (for example an unknown document version or a
readiness claim), so they are parser-compatibility tests rather than documents
the current gateway handler can emit. A separate public bridge test serializes
the real handler builder for representative enabled and unavailable states.
They also cover unsafe reason-code grammar, which both consumers reject rather
than rendering as portable capability evidence.

The corpus includes one intentional boundary case: a member cap beyond
JavaScript's `Number.MAX_SAFE_INTEGER` remains exactly representable to the
Rust CLI but must be rejected by browser/dashboard parsers rather than rounded.
That difference is explicit evidence of a current cross-language limit, not a
claim that either consumer has verified provider, model, credential, fleet, or
later-request readiness.
