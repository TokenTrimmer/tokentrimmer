use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use chrono::Utc;
use futures::{stream::BoxStream, StreamExt};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tt_shared::{
    context::BudgetDispatchState, messages::EmbeddingInput, pricing::CacheWriteTier,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};

use super::{
    BudgetDispatch, BudgetDispatchKind, BudgetReservation, BudgetReservationError,
    BudgetReservationRequest, BudgetReservationStore, ReservationAdmission, SettlementBasis,
};

/// Provider decorator installed only for a hosted database with both cap
/// tables. It is the single choke point for chat, streaming, embeddings,
/// retries, failover, panel legs, judges, and measurement calls.
pub(crate) struct BudgetedProvider {
    inner: Arc<dyn Provider>,
    store: Arc<dyn BudgetReservationStore>,
    model_limits: HashMap<String, (u64, u64)>,
}

fn canonical_request<T: Serialize>(request: &T) -> Result<String, ProviderError> {
    let value = serde_json::to_value(request)
        .map_err(|error| ProviderError::InvalidRequest(error.to_string()))?;
    serde_json::to_string(&value).map_err(|error| ProviderError::InvalidRequest(error.to_string()))
}

pub(crate) fn derive_budget_dispatch(
    state: &BudgetDispatchState,
    provider: &'static str,
    kind: BudgetDispatchKind,
    model: &str,
    canonical_request: &str,
) -> BudgetDispatch {
    let mut fingerprint = Sha256::new();
    fingerprint.update(b"tokentrimmer:budget-dispatch-fingerprint:v1\0");
    for component in [
        provider.as_bytes(),
        kind.as_str().as_bytes(),
        model.as_bytes(),
        canonical_request.as_bytes(),
    ] {
        fingerprint.update((component.len() as u64).to_be_bytes());
        fingerprint.update(component);
    }
    let fingerprint: [u8; 32] = fingerprint.finalize().into();
    let attempt = state.next_attempt(fingerprint);

    let mut key = Sha256::new();
    key.update(b"tokentrimmer:budget-dispatch-key:v1\0");
    key.update(state.seed());
    key.update(fingerprint);
    key.update(attempt.to_be_bytes());
    BudgetDispatch {
        key: key.finalize().into(),
        provider,
        kind,
    }
}

impl BudgetedProvider {
    pub(crate) fn new(inner: Arc<dyn Provider>, store: Arc<dyn BudgetReservationStore>) -> Self {
        let model_limits = inner
            .models()
            .into_iter()
            .map(|model| (model.id, (model.max_input_tokens, model.max_output_tokens)))
            .collect();
        Self {
            inner,
            store,
            model_limits,
        }
    }

    fn estimated_chat_cost(
        &self,
        req: &ChatCompletionRequest,
        canonical_request: &str,
    ) -> Option<f64> {
        let pricing = self.inner.pricing(&req.model)?;
        if pricing.input_per_million == 0.0 && pricing.output_per_million == 0.0 {
            return Some(0.0);
        }
        let estimated_input = u64::from(tt_tokenize::estimate_tokens(
            self.inner.id(),
            canonical_request,
        ));
        let model_limits = self.model_limits.get(&req.model).copied();
        let input_tokens = model_limits
            .map(|(max_input, _)| estimated_input.min(max_input))
            .unwrap_or(estimated_input);
        let output_tokens = req
            .max_completion_tokens
            .or(req.max_tokens)
            .map(u64::from)
            .or_else(|| model_limits.map(|(_, max_output)| max_output))?;
        let choices = u64::from(req.n.unwrap_or(1).max(1));
        let input_rate = pricing
            .cache_write_per_million
            .unwrap_or(pricing.input_per_million)
            .max(pricing.input_per_million);
        Some(
            ((input_tokens as f64 * input_rate)
                + (output_tokens.saturating_mul(choices) as f64 * pricing.output_per_million))
                / 1_000_000.0
                * self.inner.fee_multiplier(),
        )
    }

    fn estimated_embedding_cost(&self, req: &EmbeddingsRequest) -> Option<f64> {
        let pricing = self.inner.pricing(&req.model)?;
        let input_tokens = match &req.input {
            EmbeddingInput::Single(text) => {
                u64::from(tt_tokenize::estimate_tokens(self.inner.id(), text))
            }
            EmbeddingInput::Batch(texts) => texts
                .iter()
                .map(|text| u64::from(tt_tokenize::estimate_tokens(self.inner.id(), text)))
                .sum(),
        };
        Some(
            input_tokens as f64 * pricing.input_per_million / 1_000_000.0
                * self.inner.fee_multiplier(),
        )
    }
    fn dispatch(
        &self,
        ctx: &RequestContext,
        kind: BudgetDispatchKind,
        model: &str,
        canonical_request: &str,
    ) -> BudgetDispatch {
        derive_budget_dispatch(
            &ctx.budget_dispatch,
            self.inner.id(),
            kind,
            model,
            canonical_request,
        )
    }

    async fn reserve(
        &self,
        ctx: &RequestContext,
        dispatch: BudgetDispatch,
        model: &str,
        estimated_usd: Option<f64>,
    ) -> Result<Option<BudgetReservation>, ProviderError> {
        match self
            .store
            .reserve(BudgetReservationRequest {
                org_id: ctx.org_id,
                api_key_id: ctx.api_key_id,
                trace_id: ctx.trace_id,
                dispatch,
                model,
                estimated_usd,
                now: Utc::now(),
            })
            .await
        {
            Ok(ReservationAdmission::NotCapped) => Ok(None),
            Ok(ReservationAdmission::Reserved(reservation)) => Ok(Some(reservation)),
            Err(BudgetReservationError::Exceeded {
                estimated_usd,
                remaining_usd,
            }) => Err(ProviderError::BudgetExceeded {
                estimated_usd,
                remaining_usd,
            }),
            Err(BudgetReservationError::PriceUnknown { model }) => {
                Err(ProviderError::BudgetPriceUnknown { model })
            }
            Err(BudgetReservationError::Unavailable(message)) => {
                Err(ProviderError::BudgetUnavailable(message))
            }
        }
    }

    async fn settle_best_effort(
        &self,
        reservation: BudgetReservation,
        actual_usd: f64,
        basis: SettlementBasis,
    ) {
        if let Err(error) = self
            .store
            .settle(reservation, actual_usd, basis, Utc::now())
            .await
        {
            // Keep the provider response: returning an error after upstream
            // success invites a duplicate client retry. The active reservation
            // remains fail-closed and can be settled idempotently.
            tracing::error!(
                reservation_id = %reservation.id,
                error = %error,
                "durable budget settlement failed; reservation remains active"
            );
        }
    }

    fn actual_cost(&self, model: &str, usage: &Usage) -> Option<f64> {
        let pricing = self.inner.pricing(model)?;
        Some(usage_cost(usage, &pricing, self.inner.fee_multiplier()))
    }
}

fn usage_cost(usage: &Usage, pricing: &ModelPricing, fee_multiplier: f64) -> f64 {
    let cached = usage.cached_tokens.min(usage.prompt_tokens);
    let cache_created = usage
        .cache_creation_input_tokens
        .unwrap_or(0)
        .min(usage.prompt_tokens.saturating_sub(cached));
    let fresh = usage
        .prompt_tokens
        .saturating_sub(cached)
        .saturating_sub(cache_created);
    let cached_rate = pricing
        .cached_input_per_million
        .unwrap_or(pricing.input_per_million);
    let cache_write_rate = pricing
        .cache_write_rate_per_million(CacheWriteTier::FiveMin)
        .unwrap_or(pricing.input_per_million);
    ((fresh as f64 * pricing.input_per_million)
        + (cached as f64 * cached_rate)
        + (cache_created as f64 * cache_write_rate)
        + (usage.completion_tokens as f64 * pricing.output_per_million))
        / 1_000_000.0
        * fee_multiplier
}

struct StreamSettlementGuard {
    store: Arc<dyn BudgetReservationStore>,
    reservation: BudgetReservation,
    completed: bool,
}

impl StreamSettlementGuard {
    async fn settle(&mut self, actual_usd: f64, basis: SettlementBasis) {
        match self
            .store
            .settle(self.reservation, actual_usd, basis, Utc::now())
            .await
        {
            Ok(()) => self.completed = true,
            Err(error) => tracing::error!(
                reservation_id = %self.reservation.id,
                error = %error,
                "stream budget settlement failed; reservation remains active"
            ),
        }
    }
}

impl Drop for StreamSettlementGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let store = Arc::clone(&self.store);
        let reservation = self.reservation;
        runtime.spawn(async move {
            if let Err(error) = store
                .settle(
                    reservation,
                    reservation.estimated_usd,
                    SettlementBasis::ConservativeEstimate,
                    Utc::now(),
                )
                .await
            {
                tracing::error!(
                    reservation_id = %reservation.id,
                    error = %error,
                    "cancelled stream budget settlement failed; reservation remains active"
                );
            }
        });
    }
}

#[async_trait]
impl Provider for BudgetedProvider {
    fn id(&self) -> &'static str {
        self.inner.id()
    }

    fn models(&self) -> Vec<ModelInfo> {
        self.inner.models()
    }

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        self.inner.pricing(model)
    }

    fn fee_multiplier(&self) -> f64 {
        self.inner.fee_multiplier()
    }

    fn dropped_params(&self, req: &ChatCompletionRequest) -> Vec<String> {
        self.inner.dropped_params(req)
    }

    fn supports_response_schema(&self) -> bool {
        self.inner.supports_response_schema()
    }

    fn temperature_range(&self) -> (f32, f32) {
        self.inner.temperature_range()
    }

    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        let canonical = canonical_request(&req)?;
        let dispatch = self.dispatch(ctx, BudgetDispatchKind::Chat, &req.model, &canonical);
        let reservation = self
            .reserve(
                ctx,
                dispatch,
                &req.model,
                self.estimated_chat_cost(&req, &canonical),
            )
            .await?;
        let model = req.model.clone();
        let result = self.inner.chat_completion(req, ctx).await;
        if let Some(reservation) = reservation {
            let settlement = result
                .as_ref()
                .ok()
                .and_then(|response| self.actual_cost(&model, &response.usage))
                .map(|actual_usd| (actual_usd, SettlementBasis::ProviderUsage))
                .unwrap_or((
                    reservation.estimated_usd,
                    SettlementBasis::ConservativeEstimate,
                ));
            self.settle_best_effort(reservation, settlement.0, settlement.1)
                .await;
        }
        result
    }

    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        let canonical = canonical_request(&req)?;
        let dispatch = self.dispatch(ctx, BudgetDispatchKind::ChatStream, &req.model, &canonical);
        let reservation = self
            .reserve(
                ctx,
                dispatch,
                &req.model,
                self.estimated_chat_cost(&req, &canonical),
            )
            .await?;
        let model = req.model.clone();
        let result = self.inner.chat_completion_stream(req, ctx).await;
        let Some(reservation) = reservation else {
            return result;
        };
        let stream = match result {
            Ok(stream) => stream,
            Err(error) => {
                self.settle_best_effort(
                    reservation,
                    reservation.estimated_usd,
                    SettlementBasis::ConservativeEstimate,
                )
                .await;
                return Err(error);
            }
        };

        let store = Arc::clone(&self.store);
        let pricing = self.inner.pricing(&model);
        let fee_multiplier = self.inner.fee_multiplier();
        let guard = StreamSettlementGuard {
            store,
            reservation,
            completed: false,
        };
        Ok(futures::stream::unfold(
            (stream, guard, None::<Usage>),
            move |(mut stream, mut guard, mut usage)| {
                let pricing = pricing.clone();
                async move {
                    match stream.next().await {
                        Some(item) => {
                            if let Ok(chunk) = &item {
                                if let Some(chunk_usage) = &chunk.usage {
                                    usage = Some(chunk_usage.clone());
                                }
                            }
                            Some((item, (stream, guard, usage)))
                        }
                        None => {
                            let settlement = usage
                                .as_ref()
                                .zip(pricing.as_ref())
                                .map(|(usage, pricing)| {
                                    (
                                        usage_cost(usage, pricing, fee_multiplier),
                                        SettlementBasis::ProviderUsage,
                                    )
                                })
                                .unwrap_or((
                                    guard.reservation.estimated_usd,
                                    SettlementBasis::ConservativeEstimate,
                                ));
                            guard.settle(settlement.0, settlement.1).await;
                            None
                        }
                    }
                }
            },
        )
        .boxed())
    }

    async fn embeddings(
        &self,
        req: EmbeddingsRequest,
        ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        let canonical = canonical_request(&req)?;
        let dispatch = self.dispatch(ctx, BudgetDispatchKind::Embeddings, &req.model, &canonical);
        let reservation = self
            .reserve(
                ctx,
                dispatch,
                &req.model,
                self.estimated_embedding_cost(&req),
            )
            .await?;
        let model = req.model.clone();
        let result = self.inner.embeddings(req, ctx).await;
        if let Some(reservation) = reservation {
            let settlement = result
                .as_ref()
                .ok()
                .and_then(|response| self.actual_cost(&model, &response.usage))
                .map(|actual_usd| (actual_usd, SettlementBasis::ProviderUsage))
                .unwrap_or((
                    reservation.estimated_usd,
                    SettlementBasis::ConservativeEstimate,
                ));
            self.settle_best_effort(reservation, settlement.0, settlement.1)
                .await;
        }
        result
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        self.inner.health_check().await
    }
}
