# tokentrimmer/inspect-action

Scan your codebase for LLM token-waste patterns in CI. Posts a markdown summary to your PR's check-run.

## Quick start

```yaml
# .github/workflows/inspect.yml
name: Inspect

on:
  pull_request:
  push:
    branches: [main]

jobs:
  inspect:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: tokentrimmer/inspect-action@v1
        with:
          path: .
          fail-on: high
```

That's it. The action runs `tt inspect` over your repo, writes a markdown report to the [job step summary](https://github.blog/news-insights/product-news/supercharging-github-actions-with-job-summaries/), and fails the check if any finding meets or exceeds the `fail-on` severity.

## Inputs

| Name | Default | Description |
|---|---|---|
| `path` | `.` | Path to scan, relative to repo root. |
| `fail-on` | `high` | Severity that fails the action. One of `low`, `medium`, `high`, `critical`. |
| `output` | `(temp file)` | Write findings here. Use `.json` suffix for JSON. |
| `token` | `''` | Optional TokenTrimmer hosted API key. When set, results upload to the hosted dashboard for trend tracking. Not required — Inspect runs fully local without one. |
| `tt-version` | `latest` | `tt-cli` version to use. |

## Outputs

| Name | Description |
|---|---|
| `status` | `passed` or `failed` based on findings vs `fail-on` threshold. |

## What it detects

The 10 launch P0 rules:

| Rule | Severity |
|---|---|
| `cache-anthropic-prompt-cache-missing` | High |
| `cache-openai-prompt-cache-eligible` | Medium |
| `lib-anthropic-sdk-no-cache-control` | High |
| `model-flagship-for-classification` | Medium |
| `model-flagship-for-extraction` | Medium |
| `output-no-max-tokens` | High |
| `conversation-unbounded-history` | High |
| `agent-no-termination-condition` | Critical |
| `config-no-agents-md` | Medium |
| `config-agents-md-contains-secrets` | Critical |

Full catalog at [`docs/01-inspect-rule-catalog.md`](../docs/01-inspect-rule-catalog.md).

## License

Apache 2.0. Same as `tokentrimmer/tokentrimmer`.
