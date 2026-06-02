#!/usr/bin/env bash
# refresh-pricing.sh — Manual workflow for updating data/pricing.toml
#
# TokenTrimmer pricing is a manually-curated snapshot; there is no automated
# scraper (provider pricing pages have no stable machine-readable API).
# Run this script as a checklist reminder when updating rates.
#
# WHEN TO RUN
#   - After any provider announces a pricing change.
#   - Periodically (quarterly) to catch unannounced adjustments.
#   - When `tracing::warn!` logs fire with "model absent from pricing catalog"
#     (a new model was added that isn't yet in the catalog).
#
# HOW TO UPDATE
#
#   1. Open data/pricing.toml in your editor.
#
#   2. For each changed or new model, APPEND a new [[entry]] block below any
#      existing entries for that (provider, model). Never edit historical rows —
#      PricingCatalog::at() depends on the ordered history for replay.
#
#      Example new entry:
#
#        [[entry]]
#        provider                 = "openai"
#        model                    = "gpt-5"
#        input_per_million        = 2.50
#        output_per_million       = 10.00
#        cached_input_per_million = 1.25
#        effective_at             = "2026-09-01T00:00:00Z"   # ← real date
#
#   3. For Anthropic models, also set cache_write_per_million = 1.25 × input rate.
#      All other providers: omit that field.
#
#   4. Run the unit tests to confirm the catalog parses and the model count is
#      updated:
#
#        cargo test -p tt-shared pricing
#
#      If the "unexpected catalog size" assertion fires, update the expected
#      count in pricing.rs::embedded_catalog_parses_and_is_populated.
#
#   5. Commit data/pricing.toml (and the test update if needed) with a message
#      like: "chore(pricing): update <provider> rates effective <date>"
#
# SOURCE PAGES (as of May 2026 — update these URLs if they move)
#   OpenAI:     https://openai.com/api/pricing/
#   Anthropic:  https://www.anthropic.com/pricing#api
#   Gemini:     https://ai.google.dev/pricing
#   Groq:       https://console.groq.com/docs/openai
#   Together:   https://www.together.ai/pricing
#   Mistral:    https://mistral.ai/technology/#pricing
#   OpenRouter: https://openrouter.ai/models (per-model; check individually)
#
# FRESHNESS CHECK
#   catalog_max_effective_at() in PricingCatalog returns the newest
#   effective_at in the embedded catalog. A startup tracing::warn! fires if
#   the catalog is stale. To see the current age:
#
#     cargo test -p tt-shared pricing::catalog_tests::catalog_max_effective_at_is_present -- --nocapture

set -euo pipefail

echo "This script is a documentation/checklist tool — no automated changes."
echo "Follow the steps in the comments above to update data/pricing.toml."
echo ""
echo "Running pricing tests to show current catalog state..."
cargo test -p tt-shared pricing 2>&1 | grep -E "^(test |running |test result)"
