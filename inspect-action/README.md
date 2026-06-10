# tokentrimmer/cost-gate-action

Gate your pull requests on **projected LLM cost changes**. The action runs
`tt inspect --cost-diff` over the PR diff, posts a sticky comment with the
per-call cost delta, and (optionally) fails the check when a change would make
your LLM calls more expensive.

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

## Outputs

| Name | Description |
|---|---|
| `cost-gate` | `passed` or `failed` based on the projected per-call cost change. |
| `base-ref` | The base git ref the working tree was diffed against. |

## Report-only mode

Want the comment without blocking merges? Set `fail-on-cost-increase: false`:

```yaml
      - uses: tokentrimmer/cost-gate-action@v1
        with:
          fail-on-cost-increase: false
```

The action still posts the cost delta comment and writes the report to the job
step summary, but never fails the check.

## How costs are projected

Real prompt sizes aren't recoverable from source, so each model change is priced
against a fixed **standard profile** (1,000 input tokens / 500 output tokens)
using TokenTrimmer's local pricing catalog. The *delta between models* is what
matters — the absolute per-call figure is a comparable yardstick, not a billing
estimate. Models not in the catalog are listed but excluded from the total.

## License

Apache 2.0. Same as `tokentrimmer/tokentrimmer`.
