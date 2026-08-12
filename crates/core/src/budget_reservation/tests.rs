use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use futures::stream::BoxStream;
use tokio::sync::Mutex;
use tt_shared::{
    context::{ProviderCredentials, SecretString},
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ModelInfo, ModelPricing,
    Provider, ProviderError, RequestContext, Usage,
};

struct RecordingStore {
    deny: bool,
    reserved: Mutex<Vec<f64>>,
    dispatches: Mutex<Vec<BudgetDispatch>>,
    settled: Mutex<Vec<f64>>,
    settlement_bases: Mutex<Vec<SettlementBasis>>,
}

#[async_trait]
impl BudgetReservationStore for RecordingStore {
    async fn reserve(
        &self,
        request: BudgetReservationRequest<'_>,
    ) -> Result<ReservationAdmission, BudgetReservationError> {
        let BudgetReservationRequest {
            dispatch,
            estimated_usd,
            ..
        } = request;
        let estimated_usd = estimated_usd.expect("priced mock request");
        self.dispatches.lock().await.push(dispatch);
        self.reserved.lock().await.push(estimated_usd);
        if self.deny {
            return Err(BudgetReservationError::Exceeded {
                estimated_usd,
                remaining_usd: 0.0,
            });
        }
        Ok(ReservationAdmission::Reserved(BudgetReservation {
            id: Uuid::from_u128(1),
            estimated_usd,
        }))
    }

    async fn settle(
        &self,
        _reservation: BudgetReservation,
        actual_usd: f64,
        basis: SettlementBasis,
        _now: DateTime<Utc>,
    ) -> Result<(), BudgetReservationError> {
        self.settled.lock().await.push(actual_usd);
        self.settlement_bases.lock().await.push(basis);
        Ok(())
    }
}

struct MeteredProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl Provider for MeteredProvider {
    fn id(&self) -> &'static str {
        "openai"
    }

    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "gpt-4o-mini".to_string(),
            provider: "openai".to_string(),
            capabilities: vec![],
            max_input_tokens: 128_000,
            max_output_tokens: 16_384,
        }]
    }

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        tt_shared::pricing::catalog().latest("openai", model)
    }

    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ChatCompletionResponse {
            id: "budget-test".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: req.model,
            choices: vec![],
            usage: Usage {
                prompt_tokens: 100,
                completion_tokens: 10,
                total_tokens: 110,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        })
    }

    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Err(ProviderError::Unsupported("not used".to_string()))
    }
}

fn request_context() -> RequestContext {
    RequestContext {
        budget_dispatch: tt_shared::context::BudgetDispatchState::default(),
        trace_id: Uuid::from_u128(2),
        org_id: Uuid::from_u128(3),
        api_key_id: Uuid::from_u128(4),
        credentials: ProviderCredentials {
            api_key: SecretString::new("test"),
            base_url: None,
            extra_headers: vec![],
        },
        tag: None,
        deadline: None,
        run_id: None,
        node_id: None,
    }
}

fn bounded_request() -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: "gpt-4o-mini".to_string(),
        max_tokens: Some(100),
        ..Default::default()
    }
}

#[tokio::test]
async fn denied_reservation_prevents_provider_dispatch() {
    let provider = Arc::new(MeteredProvider {
        calls: AtomicUsize::new(0),
    });
    let store = Arc::new(RecordingStore {
        deny: true,
        reserved: Mutex::new(vec![]),
        settled: Mutex::new(vec![]),
        dispatches: Mutex::new(vec![]),
        settlement_bases: Mutex::new(vec![]),
    });
    let budgeted = BudgetedProvider::new(
        Arc::clone(&provider) as Arc<dyn Provider>,
        Arc::clone(&store) as Arc<dyn BudgetReservationStore>,
    );

    let result = budgeted
        .chat_completion(bounded_request(), &request_context())
        .await;
    assert!(matches!(result, Err(ProviderError::BudgetExceeded { .. })));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert!(store.settled.lock().await.is_empty());
}

#[tokio::test]
async fn admitted_call_settles_to_provider_reported_usage() {
    let provider = Arc::new(MeteredProvider {
        calls: AtomicUsize::new(0),
    });
    let store = Arc::new(RecordingStore {
        deny: false,
        reserved: Mutex::new(vec![]),
        settled: Mutex::new(vec![]),
        dispatches: Mutex::new(vec![]),
        settlement_bases: Mutex::new(vec![]),
    });
    let budgeted = BudgetedProvider::new(
        Arc::clone(&provider) as Arc<dyn Provider>,
        Arc::clone(&store) as Arc<dyn BudgetReservationStore>,
    );

    budgeted
        .chat_completion(bounded_request(), &request_context())
        .await
        .unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let estimated = store.reserved.lock().await[0];
    let actual = store.settled.lock().await[0];
    assert!(estimated > 0.0);
    assert!(actual > 0.0);
    assert!(actual < estimated);
    assert_eq!(
        store.settlement_bases.lock().await.as_slice(),
        &[SettlementBasis::ProviderUsage]
    );
}

#[tokio::test]
async fn dispatch_keys_are_stable_across_replay_and_distinct_across_attempts() {
    let provider = Arc::new(MeteredProvider {
        calls: AtomicUsize::new(0),
    });
    let store = Arc::new(RecordingStore {
        deny: false,
        reserved: Mutex::new(vec![]),
        dispatches: Mutex::new(vec![]),
        settled: Mutex::new(vec![]),
        settlement_bases: Mutex::new(vec![]),
    });
    let budgeted = BudgetedProvider::new(
        Arc::clone(&provider) as Arc<dyn Provider>,
        Arc::clone(&store) as Arc<dyn BudgetReservationStore>,
    );
    let mut first = request_context();
    first.budget_dispatch =
        dispatch_state_for_idempotency(first.org_id, first.api_key_id, "stable-request");
    let mut replay = request_context();
    replay.budget_dispatch =
        dispatch_state_for_idempotency(replay.org_id, replay.api_key_id, "stable-request");

    budgeted
        .chat_completion(bounded_request(), &first)
        .await
        .unwrap();
    budgeted
        .chat_completion(bounded_request(), &first)
        .await
        .unwrap();
    budgeted
        .chat_completion(bounded_request(), &replay)
        .await
        .unwrap();

    let dispatches = store.dispatches.lock().await;
    assert_ne!(dispatches[0].key, dispatches[1].key);
    assert_eq!(dispatches[0].key, dispatches[2].key);
}

#[tokio::test]
async fn missing_stream_usage_settles_with_conservative_provenance() {
    let provider = Arc::new(MeteredProvider {
        calls: AtomicUsize::new(0),
    });
    let store = Arc::new(RecordingStore {
        deny: false,
        reserved: Mutex::new(vec![]),
        dispatches: Mutex::new(vec![]),
        settled: Mutex::new(vec![]),
        settlement_bases: Mutex::new(vec![]),
    });
    let budgeted = BudgetedProvider::new(
        provider as Arc<dyn Provider>,
        Arc::clone(&store) as Arc<dyn BudgetReservationStore>,
    );

    let result = budgeted
        .chat_completion_stream(bounded_request(), &request_context())
        .await;
    assert!(matches!(result, Err(ProviderError::Unsupported(_))));
    assert_eq!(
        store.settlement_bases.lock().await.as_slice(),
        &[SettlementBasis::ConservativeEstimate]
    );
    assert_eq!(
        store.settled.lock().await.as_slice(),
        store.reserved.lock().await.as_slice()
    );
}

#[tokio::test]
#[ignore = "requires a disposable TEST_DATABASE_URL"]
async fn postgres_reservations_serialize_instances_and_survive_restart() {
    let database_url =
        std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL must be configured");
    let pool = crate::connect(&database_url, 4).await.unwrap();
    let reservation_table: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('gateway_budget_reservations')::text")
            .fetch_one(&pool)
            .await
            .unwrap();
    if reservation_table.is_none() {
        sqlx::raw_sql(include_str!(
            "../../migrations/0049_budget_reservations.up.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
    }
    let dispatch_key_column: Option<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = current_schema() \
           AND table_name = 'gateway_budget_reservations' \
           AND column_name = 'dispatch_key'",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    if dispatch_key_column.is_none() {
        sqlx::raw_sql(include_str!(
            "../../migrations/0050_budget_dispatch_provenance.up.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS org_budget_caps (\
             org_id UUID PRIMARY KEY, monthly_cap_usd DOUBLE PRECISION, \
             monthly_request_cap INTEGER, breach_policy TEXT NOT NULL DEFAULT 'block')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS api_key_budget_caps (\
             api_key_id UUID PRIMARY KEY, org_id UUID NOT NULL, \
             monthly_cap_usd DOUBLE PRECISION, monthly_request_cap INTEGER)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS request_logs (\
             id UUID PRIMARY KEY, org_id UUID NOT NULL, api_key_id UUID, \
             ts TIMESTAMPTZ NOT NULL, cost_usd DOUBLE PRECISION)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let org_id = Uuid::new_v4();
    let api_key_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO org_budget_caps (org_id, monthly_cap_usd) VALUES ($1, 1.0) \
         ON CONFLICT (org_id) DO UPDATE SET monthly_cap_usd = EXCLUDED.monthly_cap_usd",
    )
    .bind(org_id)
    .execute(&pool)
    .await
    .unwrap();
    let dispatch = |byte| BudgetDispatch {
        key: [byte; 32],
        provider: "test",
        kind: BudgetDispatchKind::Chat,
    };

    let now = DateTime::parse_from_rfc3339("2026-08-15T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let first_instance = PostgresBudgetReservationStore::new(pool.clone());
    let second_instance = PostgresBudgetReservationStore::new(pool.clone());
    let left = first_instance.reserve(BudgetReservationRequest {
        org_id,
        api_key_id,
        trace_id: Uuid::new_v4(),
        dispatch: dispatch(1),
        model: "priced",
        estimated_usd: Some(0.6),
        now,
    });
    let right = second_instance.reserve(BudgetReservationRequest {
        org_id,
        api_key_id,
        trace_id: Uuid::new_v4(),
        dispatch: dispatch(2),
        model: "priced",
        estimated_usd: Some(0.6),
        now,
    });
    let (left, right) = tokio::join!(left, right);
    let first_reservation = match (left, right) {
        (
            Ok(ReservationAdmission::Reserved(reservation)),
            Err(BudgetReservationError::Exceeded { .. }),
        )
        | (
            Err(BudgetReservationError::Exceeded { .. }),
            Ok(ReservationAdmission::Reserved(reservation)),
        ) => reservation,
        other => panic!("exactly one independent instance must reserve: {other:?}"),
    };
    first_instance
        .settle(first_reservation, 0.4, SettlementBasis::ProviderUsage, now)
        .await
        .unwrap();
    // Idempotent settlement is safe after a caller-side retry.
    second_instance
        .settle(first_reservation, 0.4, SettlementBasis::ProviderUsage, now)
        .await
        .unwrap();

    // A fresh store models process restart; it observes the durable $0.40
    // settlement and can reserve the exact remaining $0.60.
    let restarted = PostgresBudgetReservationStore::new(pool.clone());
    let (persisted_key, settlement_basis, settlement_observed_at): (
        Vec<u8>,
        Option<String>,
        Option<DateTime<Utc>>,
    ) = sqlx::query_as(
        "SELECT dispatch_key, settlement_basis, settlement_observed_at \
         FROM gateway_budget_reservations WHERE id = $1",
    )
    .bind(first_reservation.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(settlement_basis.as_deref(), Some("provider_usage"));
    assert!(settlement_observed_at.is_some());
    let replay_key: [u8; 32] = persisted_key.try_into().unwrap();
    assert!(matches!(
        restarted
            .reserve(BudgetReservationRequest {
                org_id,
                api_key_id,
                trace_id: Uuid::new_v4(),
                dispatch: BudgetDispatch {
                    key: replay_key,
                    provider: "test",
                    kind: BudgetDispatchKind::Chat,
                },
                model: "priced",
                estimated_usd: Some(0.6),
                now,
            })
            .await,
        Err(BudgetReservationError::Unavailable(_))
    ));
    let second_reservation = match restarted
        .reserve(BudgetReservationRequest {
            org_id,
            api_key_id,
            trace_id: Uuid::new_v4(),
            dispatch: dispatch(3),
            model: "priced",
            estimated_usd: Some(0.6),
            now,
        })
        .await
        .unwrap()
    {
        ReservationAdmission::Reserved(reservation) => reservation,
        ReservationAdmission::NotCapped => panic!("test org is capped"),
    };
    assert!(matches!(
        restarted
            .reserve(BudgetReservationRequest {
                org_id,
                api_key_id,
                trace_id: Uuid::new_v4(),
                dispatch: dispatch(4),
                model: "priced",
                estimated_usd: Some(0.000_001),
                now,
            })
            .await,
        Err(BudgetReservationError::Exceeded { .. })
    ));
    restarted
        .settle(second_reservation, 0.6, SettlementBasis::ProviderUsage, now)
        .await
        .unwrap();

    let adjustment_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM gateway_budget_adjustments WHERE org_id = $1")
            .bind(org_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(adjustment_count, 2);

    // An expired org reservation must be finalized even when a request
    // through a different API key is the first work after the lease.
    let stale_org_id = Uuid::new_v4();
    let stale_key_id = Uuid::new_v4();
    let discovering_key_id = Uuid::new_v4();
    sqlx::query("INSERT INTO org_budget_caps (org_id, monthly_cap_usd) VALUES ($1, 1.0)")
        .bind(stale_org_id)
        .execute(&pool)
        .await
        .unwrap();
    let stale_reservation = match restarted
        .reserve(BudgetReservationRequest {
            org_id: stale_org_id,
            api_key_id: stale_key_id,
            trace_id: Uuid::new_v4(),
            dispatch: dispatch(5),
            model: "priced",
            estimated_usd: Some(0.8),
            now: now - chrono::Duration::minutes(16),
        })
        .await
        .unwrap()
    {
        ReservationAdmission::Reserved(reservation) => reservation,
        ReservationAdmission::NotCapped => panic!("lease test org is capped"),
    };
    let discovering_reservation = match restarted
        .reserve(BudgetReservationRequest {
            org_id: stale_org_id,
            api_key_id: discovering_key_id,
            trace_id: Uuid::new_v4(),
            dispatch: dispatch(6),
            model: "priced",
            estimated_usd: Some(0.2),
            now,
        })
        .await
        .unwrap()
    {
        ReservationAdmission::Reserved(reservation) => reservation,
        ReservationAdmission::NotCapped => panic!("lease test org is capped"),
    };
    let (stale_status, stale_basis): (String, Option<String>) = sqlx::query_as(
        "SELECT status, settlement_basis FROM gateway_budget_reservations WHERE id = $1",
    )
    .bind(stale_reservation.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stale_status, "expired");
    assert_eq!(stale_basis.as_deref(), Some("lease_expiry"));
    restarted
        .settle(stale_reservation, 0.5, SettlementBasis::ProviderUsage, now)
        .await
        .unwrap();
    let (late_status, late_basis, late_settled): (String, Option<String>, Option<f64>) =
        sqlx::query_as(
            "SELECT status, settlement_basis, settled_usd \
             FROM gateway_budget_reservations WHERE id = $1",
        )
        .bind(stale_reservation.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(late_status, "settled");
    assert_eq!(late_basis.as_deref(), Some("provider_usage"));
    assert_eq!(late_settled, Some(0.5));
    restarted
        .settle(
            discovering_reservation,
            0.2,
            SettlementBasis::ProviderUsage,
            now,
        )
        .await
        .unwrap();
    let stale_adjustment_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM gateway_budget_adjustments WHERE org_id = $1")
            .bind(stale_org_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stale_adjustment_count, 3);
    sqlx::query("DELETE FROM gateway_budget_adjustments WHERE org_id = $1")
        .bind(stale_org_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM gateway_budget_reservations WHERE org_id = $1")
        .bind(stale_org_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM gateway_budget_scope_months WHERE scope_id = $1")
        .bind(stale_org_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM org_budget_caps WHERE org_id = $1")
        .bind(stale_org_id)
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query("DELETE FROM gateway_budget_adjustments WHERE org_id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM gateway_budget_reservations WHERE org_id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM gateway_budget_scope_months WHERE scope_id IN ($1, $2)")
        .bind(org_id)
        .bind(api_key_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM org_budget_caps WHERE org_id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
}

#[test]
fn calendar_month_bounds_cross_years() {
    let now = DateTime::parse_from_rfc3339("2026-12-31T23:59:59Z")
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(
        month_bounds(now),
        (
            NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(),
            NaiveDate::from_ymd_opt(2027, 1, 1).unwrap()
        )
    );
}
