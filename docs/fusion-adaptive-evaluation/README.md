# Fusion adaptive-evaluation foundation

The current runtime policy is `tokentrimmer.fusion-blind-order.v1`. Before an
LLM arbiter sees successful Synthesize or Best-of-N answers, the gateway assigns
opaque candidate numbers using a fresh request-local UUID seed and a
domain-separated SHA-256 ordering key. The key contains only the policy version,
seed, and original ordinal. It excludes model, provider, latency, cost, and
answer content.

The Public core test gate evaluates the exact runtime builder, not a duplicate
shuffle:

- For each candidate count from 2 through the default gateway maximum of 8,
  4,096 deterministic UUID seeds produce a complete source-position ×
  wire-position exposure matrix.
- Every generated order must be a permutation with no missing or duplicated
  candidate.
- Every matrix cell must remain within six binomial standard deviations of
  uniform exposure. This is a deterministic regression threshold, not a
  statistical claim about production traffic or arbiter choices.
- The versioned
  `tokentrimmer.fusion-candidate-attack-corpus.v1` fixture exercises forged
  candidate delimiters, system messages, tool calls, Markdown and XML framing,
  control characters, bidirectional text, and candidate self-identification.
  Under both LLM-arbiter tasks, each string must round-trip as the exact
  `content` value of one named `tokentrimmer.fusion-candidate-data.v1` JSON
  envelope.

This gate measures candidate exposure and wire isolation only. It does not show
that an LLM follows the instruction, resist content-based self-identification,
selects uniformly, or produces a better answer. It has no representative human
quality labels, calibrated confidence threshold, semantic contradiction
judgement, cost/latency cohort, cascade, early stop, provider run, deployment,
or production observation. Any adaptive selection policy must use a new
version, state its rollback boundary, and pass those missing evaluation gates
before it is enabled.
