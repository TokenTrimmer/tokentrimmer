//! Tests for the OpenRouter dynamic `GET /models` catalog.
//!
//! Hermetic: every test mocks the OpenRouter `/models` endpoint with
//! [`httpmock`]; there is NO real network access. They pin the three
//! behaviours the dynamic catalog must guarantee:
//!
//! 1. a mocked `/models` response is parsed into models + per-model pricing
//!    and cached (so `models()` / `pricing()` reflect the fetched catalog);
//! 2. a chat request to an OpenRouter model is priced from the fetched
//!    per-token pricing (converted to the gateway's per-million unit);
//! 3. a failed `/models` fetch leaves the static fallback list intact and
//!    never errors the provider (the refresh returns `Err`, but the provider
//!    keeps serving the static catalog).

use httpmock::prelude::*;
use tt_provider_compat::ClientConfig;
use tt_provider_openrouter::OpenRouterProvider;
use tt_shared::Provider;

/// A representative slice of the real OpenRouter `GET /models` payload.
///
/// Shape verified against the live endpoint + docs:
/// `{ "data": [ { "id", "name", "context_length", "pricing": { "prompt",
/// "completion", ... }, "top_provider": { "max_completion_tokens" }, ... } ] }`.
/// Prices are USD **per token**. The live endpoint returns them as bare JSON
/// numbers; the documented type is a string (`BigNumberUnion`) — this fixture
/// deliberately mixes both forms so the parser must tolerate each.
fn models_body() -> String {
    serde_json::json!({
        "data": [
            {
                // Numeric pricing (as the live endpoint serializes today).
                "id": "anthropic/claude-sonnet-4-6",
                "name": "Anthropic: Claude Sonnet 4.6",
                "context_length": 200_000,
                "pricing": {
                    "prompt": 0.000003,       // $3 / 1M input
                    "completion": 0.000015,   // $15 / 1M output
                    "request": 0.0,
                    "image": 0.0,
                    "input_cache_read": 0.0000003,
                    "input_cache_write": 0.00000375
                },
                "top_provider": { "max_completion_tokens": 64_000 }
            },
            {
                // String pricing (the documented BigNumberUnion form).
                "id": "deepseek/deepseek-r1",
                "name": "DeepSeek: R1",
                "context_length": 128_000,
                "pricing": {
                    "prompt": "0.00000055",   // $0.55 / 1M input
                    "completion": "0.00000219" // $2.19 / 1M output
                },
                "top_provider": { "max_completion_tokens": 8_192 }
            },
            {
                // A free model: prompt/completion = "0" → priced at zero.
                "id": "meta-llama/llama-3.3-70b-instruct:free",
                "name": "Meta: Llama 3.3 70B (free)",
                "context_length": 131_072,
                "pricing": { "prompt": "0", "completion": "0" },
                "top_provider": { "max_completion_tokens": 4_096 }
            }
        ]
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// (a) A mocked /models response is parsed into models + pricing and cached.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn refresh_parses_models_and_pricing_and_caches() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/models");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(models_body());
    });

    let provider = OpenRouterProvider::new_allow_local(ClientConfig::default());

    let count = provider
        .refresh_models_at(&server.base_url())
        .await
        .expect("refresh should succeed against the mock /models endpoint");

    mock.assert(); // the endpoint was actually hit
    assert_eq!(count, 3, "all three fixture models cached");

    // The dynamic catalog now backs models() — the three fetched ids appear.
    let ids: Vec<String> = provider.models().into_iter().map(|m| m.id).collect();
    assert!(ids.contains(&"anthropic/claude-sonnet-4-6".to_string()));
    assert!(ids.contains(&"deepseek/deepseek-r1".to_string()));
    assert!(ids.contains(&"meta-llama/llama-3.3-70b-instruct:free".to_string()));

    // Per-model pricing parsed from numeric form → per-million conversion.
    let sonnet = provider
        .pricing("anthropic/claude-sonnet-4-6")
        .expect("sonnet priced from dynamic catalog");
    assert!(
        (sonnet.input_per_million - 3.00).abs() < 1e-6,
        "got {sonnet:?}"
    );
    assert!((sonnet.output_per_million - 15.00).abs() < 1e-6);

    // Per-model pricing parsed from the string form.
    let r1 = provider
        .pricing("deepseek/deepseek-r1")
        .expect("deepseek priced from dynamic catalog");
    assert!((r1.input_per_million - 0.55).abs() < 1e-4, "got {r1:?}");
    assert!((r1.output_per_million - 2.19).abs() < 1e-4);

    // Free model → zero rates (not absent).
    let free = provider
        .pricing("meta-llama/llama-3.3-70b-instruct:free")
        .expect("free model priced (at zero)");
    assert_eq!(free.input_per_million, 0.0);
    assert_eq!(free.output_per_million, 0.0);
}

// ---------------------------------------------------------------------------
// (b) A request to an OpenRouter model is priced from the fetched pricing.
//
//     The cost path consumes `Provider::pricing(model)`; a model that is NOT
//     in the static list must become priceable once the dynamic catalog is
//     fetched. (Before refresh it is unknown; after refresh it prices.)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dynamic_model_priced_from_fetched_catalog() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(GET).path("/models");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(models_body());
    });

    let provider = OpenRouterProvider::new_allow_local(ClientConfig::default());

    // deepseek/deepseek-r1 is NOT in the static catalog → no price yet.
    assert!(
        provider.pricing("deepseek/deepseek-r1").is_none(),
        "model should be unpriced before the dynamic fetch"
    );

    provider
        .refresh_models_at(&server.base_url())
        .await
        .expect("refresh ok");

    // After the fetch the model is priced from OpenRouter's returned rates.
    let p = provider
        .pricing("deepseek/deepseek-r1")
        .expect("now priced from the fetched catalog");
    assert!((p.input_per_million - 0.55).abs() < 1e-4);
    assert!((p.output_per_million - 2.19).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// (c) A failed /models fetch falls back to the static list without erroring.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failed_refresh_falls_back_to_static_without_panicking() {
    let server = MockServer::start();
    // Upstream returns 500 — the refresh must surface an Err, NOT panic, and
    // must leave the static catalog serving.
    let _mock = server.mock(|when, then| {
        when.method(GET).path("/models");
        then.status(500)
            .header("Content-Type", "application/json")
            .body(r#"{"error":"internal"}"#);
    });

    let provider = OpenRouterProvider::new_allow_local(ClientConfig::default());

    // Static baseline before any (attempted) refresh.
    let static_models = provider.models();
    assert!(
        !static_models.is_empty(),
        "static catalog should be non-empty"
    );
    let static_sonnet = provider
        .pricing("anthropic/claude-sonnet-4.6")
        .expect("static sonnet priced");

    // The refresh fails — but returns Err rather than panicking / poisoning.
    let result = provider.refresh_models_at(&server.base_url()).await;
    assert!(result.is_err(), "a 500 should produce an Err");

    // The provider still serves the static catalog unchanged.
    let after = provider.models();
    assert_eq!(
        after.len(),
        static_models.len(),
        "static catalog intact after a failed refresh"
    );
    let sonnet_after = provider
        .pricing("anthropic/claude-sonnet-4.6")
        .expect("static sonnet still priced after failed refresh");
    assert_eq!(
        sonnet_after.input_per_million,
        static_sonnet.input_per_million
    );
    assert_eq!(
        sonnet_after.output_per_million,
        static_sonnet.output_per_million
    );
}
