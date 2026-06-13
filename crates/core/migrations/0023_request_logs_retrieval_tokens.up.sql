-- Token-denominated retrieval substitution accounting.
--
-- `retrieval_tokens_saved` is the net token delta from `<retrievable>` prompt
-- substitution: original tag payload tokens minus retrieved replacement tokens,
-- minus the query-embedding token cost. It may be negative and is deliberately
-- NOT converted to USD or folded into cost_usd / baseline_cost_usd / saved-usd.
ALTER TABLE request_logs
  ADD COLUMN retrieval_tokens_saved BIGINT NOT NULL DEFAULT 0;
