//! Request-local retrieval intent. Middleware can authorize tenant identity,
//! but only chat preparation can execute after resolving the selected policy.
use std::sync::Arc;

use axum::{http::HeaderValue, response::Response};
use tokio::sync::Mutex;
use tt_shared::{ChatCompletionRequest, Message, MessageContent};
use uuid::Uuid;

use super::{RetrievalState, RetrievalTelemetry};
use crate::{ApiError, ApiResult};

#[derive(Clone, Copy, Default)]
enum Outcome {
    #[default]
    Pending,
    Applied {
        telemetry: RetrievalTelemetry,
        low_confidence: u32,
    },
    Failed(&'static str),
}

/// Only constructed from trusted request extensions, never from a header or
/// prompt-supplied org id. Clones share bounded outcome metadata (no prompts).
#[derive(Clone)]
pub struct DeferredRetrieval {
    state: RetrievalState,
    org_id: Uuid,
    outcome: Arc<Mutex<Outcome>>,
}

impl DeferredRetrieval {
    pub(super) fn new(state: RetrievalState, org_id: Uuid) -> Self {
        Self {
            state,
            org_id,
            outcome: Arc::new(Mutex::new(Outcome::Pending)),
        }
    }

    pub(crate) fn org_id(&self) -> Uuid {
        self.org_id
    }

    /// Called only after routing succeeds. The selected redaction decision
    /// guards ALL embedding queries. Retrieved context is then subject to the
    /// existing outbound request-redaction stage, using the SAME route match.
    pub(crate) async fn apply(
        &self,
        req: &mut ChatCompletionRequest,
        org_id: Uuid,
        redact: bool,
    ) -> ApiResult<RetrievalTelemetry> {
        if org_id != self.org_id() {
            return Err(ApiError::Forbidden(
                "retrieval identity does not match request identity".into(),
            ));
        }
        // Transactional mutation: never expose a partially substituted request
        // if a later message's embedding/search fails.
        let mut messages = match serde_json::to_value(&req.messages) {
            Ok(serde_json::Value::Array(messages)) => messages,
            _ => return Ok(self.fail("serialize-failed").await),
        };
        // The original-body audit predates route privacy. Do not retain raw
        // caller text on redact routes. Other routes keep the existing explicit
        // encrypted-audit behavior, now with a canonical pre-substitution body.
        let audit_prompt = if !redact && self.state.audit.is_some() {
            serde_json::to_string(req).ok().map(zeroize::Zeroizing::new)
        } else {
            None
        };
        let result = tt_retrieval::substitute::substitute_in_messages_with_query_transform(
            &mut messages,
            org_id,
            self.state.store.as_ref(),
            &self.state.embedder,
            |query| {
                if redact {
                    crate::passes::redaction::redact_query(query)
                } else {
                    query.to_owned()
                }
            },
        )
        .await;
        let report = match result {
            Ok(report) => report,
            Err(error) => {
                let kind = match error {
                    tt_retrieval::RetrievalError::Embedding(_) => "embedding-error",
                    tt_retrieval::RetrievalError::Store(_) => "store-error",
                    tt_retrieval::RetrievalError::Tag(_) => "tag-parse-error",
                    tt_retrieval::RetrievalError::Malformed(_) => "malformed",
                    tt_retrieval::RetrievalError::InvalidEmbedding => "invalid-embedding",
                };
                // Error text can contain upstream response bodies. Log only a
                // bounded class, never the query, original body or retrieved text.
                tracing::warn!(
                    kind,
                    "retrieval failed; preserving pre-substitution messages"
                );
                return Ok(self.fail(kind).await);
            }
        };
        let mut updated: Vec<Message> =
            match serde_json::from_value(serde_json::Value::Array(messages)) {
                Ok(messages) => messages,
                Err(_) => return Ok(self.fail("serialize-failed").await),
            };
        if redact {
            // The ordinary request guard intentionally leaves assistant history
            // alone. Newly retrieved assistant content is NOT model output:
            // sanitize that replacement here, without rewriting untouched
            // assistant messages or crediting redaction as retrieval savings.
            for (before, after) in req.messages.iter().zip(&mut updated) {
                if let (
                    Message::Assistant {
                        content: Some(MessageContent::Text(before)),
                        ..
                    },
                    Message::Assistant {
                        content: Some(MessageContent::Text(after)),
                        ..
                    },
                ) = (before, after)
                {
                    if before != after {
                        *after = crate::passes::redaction::redact_query(after);
                    }
                }
            }
        }
        req.messages = updated;
        let telemetry = RetrievalTelemetry {
            substitutions: report.substitutions,
            tokens_saved: report.tokens_saved_estimate,
        };
        *self.outcome.lock().await = Outcome::Applied {
            telemetry,
            low_confidence: report.low_confidence_skips,
        };
        if let (Some(audit), Some(prompt)) = (self.state.audit.clone(), audit_prompt) {
            tokio::spawn(async move {
                if audit
                    .record(
                        org_id,
                        telemetry.substitutions,
                        telemetry.tokens_saved,
                        prompt.as_str(),
                    )
                    .await
                    .is_err()
                {
                    tracing::warn!("retrieval audit record failed");
                }
            });
        }
        Ok(telemetry)
    }

    async fn fail(&self, kind: &'static str) -> RetrievalTelemetry {
        *self.outcome.lock().await = Outcome::Failed(kind);
        RetrievalTelemetry::default()
    }

    pub(super) async fn attach_headers(&self, response: &mut Response) {
        let outcome = *self.outcome.lock().await;
        let headers = response.headers_mut();
        match outcome {
            Outcome::Pending => {
                headers.insert(
                    "x-tokentrimmer-retrieval-skipped",
                    HeaderValue::from_static("policy-not-reached"),
                );
            }
            Outcome::Failed(kind) => {
                headers.insert(
                    "x-tokentrimmer-retrieval-error",
                    HeaderValue::from_static(kind),
                );
            }
            Outcome::Applied {
                telemetry,
                low_confidence,
            } => {
                headers.insert(
                    "x-tokentrimmer-retrieval-enabled",
                    HeaderValue::from_static("active"),
                );
                for (name, value) in [
                    (
                        "x-tokentrimmer-retrieval-substitutions",
                        telemetry.substitutions.to_string(),
                    ),
                    (
                        "x-tokentrimmer-retrieval-tokens-saved",
                        telemetry.tokens_saved.to_string(),
                    ),
                ] {
                    if let Ok(value) = HeaderValue::from_str(&value) {
                        headers.insert(name, value);
                    }
                }
                if low_confidence > 0 {
                    if let Ok(value) = HeaderValue::from_str(&low_confidence.to_string()) {
                        headers.insert("x-tokentrimmer-retrieval-low-confidence", value);
                    }
                }
            }
        }
    }
}
