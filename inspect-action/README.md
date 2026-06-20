# tokentrimmer/cost-gate-action

Gate your pull requests on **projected LLM cost changes**. The action runs
`tt inspect --cost-diff` over the PR diff, posts a sticky comment with the
per-call cost delta, and (optionally) fails the check when a change would make
your LLM calls more expensive.

> **Which `uses:` ref?** `tokentrimmer/cost-gate-action` is the intended
> standalone repo for this action once it's published to the Marketplace — it
> does **not** exist yet. Until then the action ships inside the monorepo, so
> consume it via the subpath form:
> `uses: TokenTrimmer/tokentrimmer/inspect-action@<ref>` (pin `<ref>` to a tag
> or commit SHA). The `tokentrimmer/cost-gate-action@v1` snippets below are
> written against the future standalone repo; swap in the subpath ref to run
> the action today.

It works by reading the model identifiers (`model = "..."`, `"model": "..."`,
etc.) added and removed in `git diff <base> -- <path>`, pricing each against
TokenTrimmer's local model catalog, and reporting the net projected per-call
cost change. Swapping `gpt-4o` → `gpt-4o-mini` shows up as a saving; adding an
expensive model shows up as a regression. No network, no hosted account
required.

## Quick start

```yaml
# .github/workflows/cost-gate.yml
name: Cost gate

on:
  pull_request:

permissions:
  contents: read
  pull-requests: write # required for the PR comment

jobs:
  cost-gate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0 # needed so the base ref is available for the diff
      # Until the standalone repo is published, use the monorepo subpath:
      #   uses: TokenTrimmer/tokentrimmer/inspect-action@v1
      - uses: tokentrimmer/cost-gate-action@v1
        with:
          path: .
          fail-on-cost-increase: true
```

That's it. On every pull request the action diffs your working tree against the
PR's base, prices the model changes, posts a comment like:

> ## 💸 TokenTrimmer cost-diff
>
> ⚠️ **Projected cost increase: +$0.007050/call** (standard profile: 1000 in / 500 out)
>
> | Model | Provider | +added | −removed | $/call (std) |
> |---|---|--:|--:|--:|
> | `gpt-4o` | openai | 1 | 0 | $0.007500 |
> | `gpt-4o-mini` | openai | 0 | 1 | $0.000450 |

…and fails the check if the net change is an increase.

> **Note:** `fetch-depth: 0` on `actions/checkout` is required so the base ref
> is present in the local git history for the diff.

## Inputs

| Name | Default | Description |
|---|---|---|
| `path` | `.` | Path to scope the cost diff to, relative to repo root. |
| `base-ref` | PR base SHA, else `HEAD` | Base git ref to diff the working tree against. |
| `fail-on-cost-increase` | `true` | Fail the check on a projected per-call cost increase. Set `false` to report only. |
| `comment` | `true` | Post/update a sticky PR comment with the cost-diff report (pull requests only). |
| `github-token` | `${{ github.token }}` | Token used to post the PR comment. |
| `tt-version` | `latest` | `tt-cli` version to use. |
| `upload-sarif` | `false` | Also run the Inspect static-analysis rules over `path` and upload the results as SARIF 2.1.0 to the Code Scanning / Security tab. Requires `permissions: security-events: write`. |
| `sarif-fail-on` | `critical` | With `upload-sarif: true`, the minimum finding severity that fails the job (`low`\|`medium`\|`high`\|`critical`). SARIF is still produced/uploaded regardless. |

## Outputs

| Name | Description |
|---|---|
| `cost-gate` | `passed` or `failed` based on the projected per-call cost change. |
| `base-ref` | The base git ref the working tree was diffed against. |
| `findings-gate` | With `upload-sarif: true`, `passed`/`failed` based on whether any finding met `sarif-fail-on`. Empty when SARIF is disabled. |

## SARIF / Code Scanning (opt-in)

Set `upload-sarif: true` to additionally run the Inspect static-analysis rules
and surface findings in the GitHub **Security → Code Scanning** tab plus inline
PR annotations. This is independent of the cost-diff gate above.

```yaml
permissions:
  contents: read
  pull-requests: write   # cost-diff comment
  security-events: write # required to upload SARIF

jobs:
  inspect:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      # Monorepo subpath today: TokenTrimmer/tokentrimmer/inspect-action@v1
      - uses: tokentrimmer/cost-gate-action@v1
        with:
          path: .
          upload-sarif: true
          sarif-fail-on: high   # gate on high+; upload everything
```

Under the hood the action runs `tt inspect --format sarif > results.sarif` and
uploads it with `github/codeql-action/upload-sarif`. The CLI emits clean SARIF
2.1.0 to stdout (severity → SARIF level: critical/high → `error`, medium →
`warning`, low → `note`); the finding's confidence and fix hint ride along in
each result's `properties`.

On **fork-contributor PRs** the token is read-only and cannot upload SARIF:
the upload step warns-and-continues (it never red-Xes the job), and the
`sarif-fail-on` findings gate is still applied independently from the generated
SARIF — so gating keeps working even when the Security-tab upload is denied.

## Report-only mode

Want the comment without blocking merges? Set `fail-on-cost-increase: false`:

```yaml
      # Or, from the monorepo today:
      #   uses: TokenTrimmer/tokentrimmer/inspect-action@v1
      - uses: tokentrimmer/cost-gate-action@v1
        with:
          fail-on-cost-increase: false
```

The action still posts the cost delta comment and writes the report to the job
step summary, but never fails the check.

## Fork-PR / read-only token note

On pull requests opened from a **fork**, GitHub gives the workflow a read-only
`GITHUB_TOKEN`, so the action cannot post or update the PR comment. This is by
design and is **not** a failure: the comment step warns and continues, the
cost-diff report is still written to the **job step summary**, and the
pass/fail gate (`fail-on-cost-increase`) is decided independently — so the gate
keeps working even when commenting is denied. Set `comment: false` to skip the
comment step entirely. To get comments on fork PRs, run the gate from a
`pull_request_target` workflow (note the
[security implications](https://securitylab.github.com/research/github-actions-preventing-pwn-requests/))
or post via a separate workflow with a write-scoped token.

## How costs are projected

Real prompt sizes aren't recoverable from source, so each model change is priced
against a fixed **standard profile** (1,000 input tokens / 500 output tokens)
using TokenTrimmer's local pricing catalog. The *delta between models* is what
matters — the absolute per-call figure is a comparable yardstick, not a billing
estimate. Models not in the catalog are listed but excluded from the total.

## License

Apache 2.0. Same as `tokentrimmer/tokentrimmer`.
