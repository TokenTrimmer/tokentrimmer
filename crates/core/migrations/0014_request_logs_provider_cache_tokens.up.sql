-- Provider prompt-cache telemetry (research Phase 0.2): persist the RAW
-- provider-reported cache token counts alongside provider_cache_saved_usd
-- (migration 0011) so per-route compress-vs-cache decisions can be made from
-- request_logs alone. Both ADDITIVE + NULLABLE (no default/backfill):
--   * cache_read_input_tokens     — tokens read from the provider's prompt
--     cache (Anthropic cache_read_input_tokens; OpenAI
--     prompt_tokens_details.cached_tokens; Gemini cachedContentTokenCount).
--   * cache_creation_input_tokens — tokens written to the provider's prompt
--     cache (Anthropic only; OpenAI/Gemini never report writes -> NULL).
-- NULL != 0: NULL = provider did not report the field, or no provider call
-- was made (TT L1/L2 hits). 0 = provider explicitly reported zero. The
-- NOT NULL cached_tokens column keeps its folded (absent => 0) semantics.
ALTER TABLE request_logs
  ADD COLUMN cache_read_input_tokens     INT NULL,
  ADD COLUMN cache_creation_input_tokens INT NULL;
