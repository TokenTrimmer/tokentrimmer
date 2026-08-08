//! Langfuse trace deep links, built purely from the OTLP trace id the gateway
//! already exports — no network calls, no vendor credentials.
//!
//! ## What this is
//!
//! [`LangfuseTraceLink`] is a validation wrapper around a Langfuse destination
//! (a bare `https` origin plus an optional project id) and a pure function from
//! a retained request trace identity ([`RetainedTrace`] — the gateway's own
//! request-correlation id plus the OTel trace/span ids it exported for the
//! request span) to an HTTPS deep link into that destination.
//!
//! The link is built from the **OTel `trace_id`** — the 32-char lowercase hex
//! W3C/OTLP trace id the request span exported (the same id that flows through
//! the gateway's existing [`crate::propagation`] `traceparent` ingest and the
//! OTLP exporter in [`crate::tracing`]). An OTLP trace formed from spans
//! sharing that id is what the vendor shows; Langfuse deep links are
//! trace-scoped, so the span id never appears in the URL (it is kept on
//! [`RetainedTrace`] only as the join key tying the link back to the gateway's
//! request logs).
//!
//! ## Constraints (all enforced, all unit-tested)
//!
//! * **No credentials.** The destination is a bare `https` origin — userinfo
//!   (`scheme://user:pass@host`) is rejected at construction, and nothing here
//!   ever fetches or otherwise contacts Langfuse. The only inputs are values
//!   the gateway already retains.
//! * **HTTPS only.** `http://` and any non-`https` scheme are rejected.
//! * **Bounded.** A valid OTLP trace id is exactly 32 hex chars, the origin and
//!   project id are bounded at construction, and [`MAX_TRACE_LINK_LEN`] is a
//!   defense-in-depth cap on the generated URL.
//! * **No malformed links.** An absent, empty, or non-conforming OTel trace id
//!   (or an empty request correlation id) yields [`Option::None`] — there is
//!   never a "link" built on bad input.
//!
//! ## URL shapes
//!
//! * Without a project id — the host-scoped deep link (what an operator gets
//!   when only the OTLP trace id is known, no project metadata):
//!   `https://cloud.langfuse.com/trace/{otlp_trace_id}`.
//! * With a project id — the project-scoped deep link Langfuse surfaces in its
//!   UI/SDKs:
//!   `https://{host}/project/{project_id}/traces/{otlp_trace_id}`.
//!
//! The default destination is [`DEFAULT_LANGFUSE_HOST`]
//! (`https://cloud.langfuse.com`, the EU-region primary cloud); US/JP/HIPAA and
//! self-hosted instances pass their own origin to [`LangfuseTraceLink::new`]
//! and, when known, a project id via [`LangfuseTraceLink::with_project_id`].
//!
//! ## Not in scope
//!
//! Hosted **per-org fanout** (which org's traces go to which vendor),
//! **delivery evidence** (confirming a vendor received a trace), LangSmith run
//! links (run-scoped, requiring org/project/run ids that are not derivable from
//! a pure OTel trace id), and eval import remain residuals of the
//! observability-integration item; this module adds no network calls and no
//! cross-org state.

use opentelemetry::trace::{SpanId, TraceId};
use url::Url;

/// Default Langfuse destination — the EU-region primary cloud. This mirrors the
/// origin an operator's gateway OTLP exporter (`OTEL_EXPORTER_OTLP_ENDPOINT`)
/// targets when using Langfuse Cloud.
pub const DEFAULT_LANGFUSE_HOST: &str = "https://cloud.langfuse.com";

/// Defense-in-depth cap on a generated deep link length. A well-formed link is
/// at most ~250 chars (origin bounded by DNS rules, project id ≤
/// [`MAX_PROJECT_ID_LEN`], trace id exactly 32), so this bound is only
/// reachable if a component somehow bypassed the stricter checks above.
pub const MAX_TRACE_LINK_LEN: usize = 512;

/// Upper bound on a Langfuse project id segment. Langfuse project ids are
/// opaquely generated alphanumeric ids (e.g. `clkpwwm0m000gmm094odg11gi`); the
/// bound keeps URLs short and the charset restriction below prevents
/// path-separator / credential smuggling.
pub const MAX_PROJECT_ID_LEN: usize = 64;

/// OTLP trace id width — a 16-byte W3C trace id renders as exactly 32 hex
/// chars.
const OTLP_TRACE_ID_HEX_LEN: usize = 32;

/// Errors from [`LangfuseTraceLink`] construction.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TraceLinkError {
    #[error("Langfuse destination is not a valid URL")]
    InvalidOrigin,
    #[error("Langfuse destination must use the https scheme (got {0:?})")]
    NonHttps(String),
    #[error("Langfuse destination must not embed userinfo credentials")]
    HasUserInfo,
    #[error("Langfuse destination must be a bare https origin (no path, query, or fragment)")]
    NonOrigin,
    #[error("Langfuse project id must be 1..={MAX_PROJECT_ID_LEN} chars of [A-Za-z0-9_-]")]
    InvalidProjectId,
}

/// A validated, credential-free Langfuse destination for trace deep links.
///
/// Construct with [`LangfuseTraceLink::new`] (a bare `https` origin), augment
/// with [`LangfuseTraceLink::with_project_id`] when a project id is known, and
/// build links with [`LangfuseTraceLink::deep_link`] / `trace_url`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LangfuseTraceLink {
    /// Normalized bare `https://host[:port]` origin — no trailing slash, no
    /// userinfo, no path/query/fragment.
    origin: String,
    /// Optional Langfuse project id, scoping links under
    /// `/project/{id}/traces/`.
    project_id: Option<String>,
}

impl LangfuseTraceLink {
    /// Validate an operator-supplied Langfuse origin and turn it into a
    /// destination. Accepts only a bare `https` origin: userinfo (credentials),
    /// non-`https` schemes, and non-origin suffixes (paths, queries, fragments)
    /// are rejected, so a generated link can never leak credentials or point
    /// somewhere other than the vendor's origin.
    pub fn new(origin: &str) -> Result<Self, TraceLinkError> {
        let url = Url::parse(origin).map_err(|_| TraceLinkError::InvalidOrigin)?;
        Self::from_parsed(url)
    }

    fn from_parsed(url: Url) -> Result<Self, TraceLinkError> {
        if url.scheme() != "https" {
            return Err(TraceLinkError::NonHttps(url.scheme().to_owned()));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(TraceLinkError::HasUserInfo);
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(TraceLinkError::NonOrigin);
        }
        // `path()` is "/" for a bare hierarchical origin; anything beyond that
        // is not a valid destination.
        match url.path() {
            "" | "/" => {}
            _ => return Err(TraceLinkError::NonOrigin),
        }
        let host = url.host_str().ok_or(TraceLinkError::InvalidOrigin)?;

        // Rebuild `scheme://host[:port]` from the parsed parts (never from
        // `Url::authority`, which includes userinfo) so the stored origin is
        // exactly what we validated.
        let mut origin = format!("https://{host}");
        if let Some(port) = url.port() {
            if port != 443 {
                origin.push_str(&format!(":{port}"));
            }
        }
        Ok(Self { origin, project_id: None })
    }

    /// Attach a Langfuse project id so generated links are project-scoped
    /// (`/project/{id}/traces/{trace_id}`). The id is validated against a
    /// bounded `[A-Za-z0-9_-]` charset so it needs no percent-encoding and
    /// cannot smuggle path separators or credentials.
    pub fn with_project_id(mut self, project_id: &str) -> Result<Self, TraceLinkError> {
        if project_id.is_empty()
            || project_id.len() > MAX_PROJECT_ID_LEN
            || !project_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(TraceLinkError::InvalidProjectId);
        }
        self.project_id = Some(project_id.to_owned());
        Ok(self)
    }

    /// Build a deep link for a retained request trace.
    ///
    /// Returns `None` — never a malformed URL — when the request correlation id
    /// is absent/blank or the trace exported no OTel trace id.
    pub fn deep_link(&self, trace: &RetainedTrace) -> Option<String> {
        if trace.request_trace_id.trim().is_empty() {
            return None;
        }
        self.trace_url_from_hex(trace.otel_trace_id.as_deref()?)
    }

    /// Build a deep link from an OTel [`TraceId`] value directly (e.g. from a
    /// span context). Returns `None` when the trace id is invalid (all-zero).
    pub fn trace_url(&self, trace_id: TraceId) -> Option<String> {
        self.trace_url_from_hex(&trace_id.to_string())
    }

    /// Build a deep link from a 32-hex OTel trace id string (the exact
    /// lowercase wire form). Returns `None` — never a malformed URL — for an
    /// absent, wrong-length, non-hex, uppercase, or all-zero id.
    pub fn trace_url_from_hex(&self, otel_trace_id: &str) -> Option<String> {
        if !is_valid_otlp_trace_id(otel_trace_id) {
            return None;
        }
        let url = match &self.project_id {
            Some(project_id) => {
                format!("{}/project/{project_id}/traces/{otel_trace_id}", self.origin)
            }
            None => format!("{}/trace/{otel_trace_id}", self.origin),
        };
        // Defense-in-depth: never emit an over-long link.
        (url.len() <= MAX_TRACE_LINK_LEN).then_some(url)
    }
}

impl Default for LangfuseTraceLink {
    fn default() -> Self {
        Self::new(DEFAULT_LANGFUSE_HOST)
            .expect("DEFAULT_LANGFUSE_HOST is a valid bare https origin (unit-tested)")
    }
}

/// The trace identity the gateway already retains for one request, with no
/// vendor involvement — the values a request-log row / dashboard entry has
/// before any vendor is consulted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedTrace {
    /// The gateway's own request-correlation id (`x-tokentrimmer-trace-id` on
    /// responses, `tokentrimmer.trace_id` on exported spans).
    pub request_trace_id: String,
    /// The OTel/W3C trace id the request span exported, as its 32-char
    /// lowercase hex, or `None` when nothing was exported.
    pub otel_trace_id: Option<String>,
    /// The OTel span id for the request span (16-char lowercase hex), or
    /// `None`. Informational: Langfuse deep links are trace-scoped, so the span
    /// id is not part of the URL.
    pub otel_span_id: Option<String>,
}

impl RetainedTrace {
    /// Build from the raw strings the gateway already retains.
    pub fn new(
        request_trace_id: impl Into<String>,
        otel_trace_id: Option<&str>,
        otel_span_id: Option<&str>,
    ) -> Self {
        Self {
            request_trace_id: request_trace_id.into(),
            otel_trace_id: otel_trace_id.map(Into::into),
            otel_span_id: otel_span_id.map(Into::into),
        }
    }

    /// Build from real OTel id values (e.g. a
    /// [`opentelemetry::trace::SpanContext`]); the hex strings
    /// are rendered from the id types so they always match the wire form, and
    /// invalid (all-zero) ids are dropped to `None`.
    pub fn from_otel(
        request_trace_id: impl Into<String>,
        otel_trace_id: Option<TraceId>,
        otel_span_id: Option<SpanId>,
    ) -> Self {
        Self {
            request_trace_id: request_trace_id.into(),
            otel_trace_id: otel_trace_id
                .filter(|id| *id != TraceId::INVALID)
                .map(|id| id.to_string()),
            otel_span_id: otel_span_id
                .filter(|id| *id != SpanId::INVALID)
                .map(|id| id.to_string()),
        }
    }
}

/// An OTLP trace id is valid for linking when it is exactly the 32-char
/// lowercase hex wire form and not the all-zero (invalid-per-spec) value.
fn is_valid_otlp_trace_id(hex: &str) -> bool {
    hex.len() == OTLP_TRACE_ID_HEX_LEN
        && hex.bytes().any(|b| b != b'0')
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W3C TraceContext §3.2.1 example trace id (32 lowercase hex chars).
    const TRACE_ID_HEX: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
    /// W3C TraceContext §3.2.1 example parent-id (16 lowercase hex chars).
    const SPAN_ID_HEX: &str = "00f067aa0ba902b7";
    /// A Langfuse-shaped project id.
    const PROJECT_ID: &str = "clkpwwm0m000gmm094odg11gi";
    /// The gateway's request-correlation id shape (`x-tokentrimmer-trace-id`).
    const REQUEST_TRACE_ID: &str = "018f27e1-0000-7000-8000-000000000001";

    /// A retained identity carrying a request correlation id + the spec example
    /// OTel span id, with the OTel trace id supplied per test.
    fn retained(otel_trace_id: Option<&str>) -> RetainedTrace {
        RetainedTrace::new(REQUEST_TRACE_ID, otel_trace_id, Some(SPAN_ID_HEX))
    }

    // ---- Exact trace-id → vendor URL shape ----

    #[test]
    fn deep_link_shape_host_scoped() {
        let link = LangfuseTraceLink::new(DEFAULT_LANGFUSE_HOST).unwrap();
        let url = link
            .deep_link(&retained(Some(TRACE_ID_HEX)))
            .expect("valid trace id must produce a link");
        assert_eq!(
            url,
            "https://cloud.langfuse.com/trace/4bf92f3577b34da6a3ce929d0e0e4736"
        );
    }

    #[test]
    fn deep_link_shape_project_scoped() {
        let link = LangfuseTraceLink::new("https://us.cloud.langfuse.com")
            .unwrap()
            .with_project_id(PROJECT_ID)
            .unwrap();
        let url = link
            .deep_link(&retained(Some(TRACE_ID_HEX)))
            .expect("valid trace id must produce a link");
        assert_eq!(
            url,
            "https://us.cloud.langfuse.com/project/clkpwwm0m000gmm094odg11gi/traces/\
             4bf92f3577b34da6a3ce929d0e0e4736"
        );
    }

    #[test]
    fn self_hosted_origin_and_port_are_preserved() {
        let link = LangfuseTraceLink::new("https://langfuse.internal:3000").unwrap();
        let url = link
            .trace_url_from_hex(TRACE_ID_HEX)
            .expect("valid trace id must produce a link");
        assert_eq!(
            url,
            "https://langfuse.internal:3000/trace/4bf92f3577b34da6a3ce929d0e0e4736"
        );

        // Default https port is normalized away.
        let link = LangfuseTraceLink::new("https://langfuse.internal:443").unwrap();
        let url = link.trace_url_from_hex(TRACE_ID_HEX).unwrap();
        assert_eq!(
            url,
            "https://langfuse.internal/trace/4bf92f3577b34da6a3ce929d0e0e4736"
        );
    }

    #[test]
    fn from_otel_renders_wire_hex_and_defaults_host() {
        let trace_id = TraceId::from_hex(TRACE_ID_HEX).unwrap();
        let span_id = SpanId::from_hex(SPAN_ID_HEX).unwrap();
        let trace = RetainedTrace::from_otel(REQUEST_TRACE_ID, Some(trace_id), Some(span_id));
        assert_eq!(trace.otel_trace_id.as_deref(), Some(TRACE_ID_HEX));
        assert_eq!(trace.otel_span_id.as_deref(), Some(SPAN_ID_HEX));

        let url = LangfuseTraceLink::default().deep_link(&trace).unwrap();
        assert_eq!(
            url,
            "https://cloud.langfuse.com/trace/4bf92f3577b34da6a3ce929d0e0e4736"
        );
    }

    // ---- Safety: https only, no credentials, bounded ----

    #[test]
    fn rejects_non_https_schemes() {
        assert_eq!(
            LangfuseTraceLink::new("http://cloud.langfuse.com"),
            Err(TraceLinkError::NonHttps("http".to_owned()))
        );
        assert_eq!(
            LangfuseTraceLink::new("ftp://cloud.langfuse.com"),
            Err(TraceLinkError::NonHttps("ftp".to_owned()))
        );
    }

    #[test]
    fn rejects_unparsable_origins() {
        assert_eq!(LangfuseTraceLink::new(""), Err(TraceLinkError::InvalidOrigin));
        assert_eq!(
            LangfuseTraceLink::new("not a url"),
            Err(TraceLinkError::InvalidOrigin)
        );
        assert_eq!(
            LangfuseTraceLink::new("https://"),
            Err(TraceLinkError::InvalidOrigin)
        );
    }

    #[test]
    fn rejects_embedded_credentials() {
        // Password (userinfo) is rejected outright.
        assert_eq!(
            LangfuseTraceLink::new("https://user:secret@cloud.langfuse.com"),
            Err(TraceLinkError::HasUserInfo)
        );
        // Even a username-only userinfo would carry a credential-shaped value.
        assert_eq!(
            LangfuseTraceLink::new("https://apikey@cloud.langfuse.com"),
            Err(TraceLinkError::HasUserInfo)
        );
    }

    #[test]
    fn rejects_paths_queries_and_fragments() {
        assert_eq!(
            LangfuseTraceLink::new("https://cloud.langfuse.com/api"),
            Err(TraceLinkError::NonOrigin)
        );
        assert_eq!(
            LangfuseTraceLink::new("https://cloud.langfuse.com?project=x"),
            Err(TraceLinkError::NonOrigin)
        );
        assert_eq!(
            LangfuseTraceLink::new("https://cloud.langfuse.com/#frag"),
            Err(TraceLinkError::NonOrigin)
        );
    }

    #[test]
    fn generated_urls_are_bounded() {
        let link = LangfuseTraceLink::new("https://cloud.langfuse.com")
            .unwrap()
            .with_project_id(PROJECT_ID)
            .unwrap();
        let url = link
            .deep_link(&retained(Some(TRACE_ID_HEX)))
            .expect("valid trace id must produce a link");
        assert!(
            url.len() <= MAX_TRACE_LINK_LEN,
            "link length {len} exceeds {MAX_TRACE_LINK_LEN}: {url}",
            len = url.len()
        );
        // The realistic ceiling is far below the cap.
        assert!(url.len() < 200, "unexpectedly long link: {url}");
    }

    #[test]
    fn refuses_overlong_or_hostile_project_ids() {
        let bare = || LangfuseTraceLink::new("https://cloud.langfuse.com").unwrap();

        let overlong = "x".repeat(MAX_PROJECT_ID_LEN + 1);
        assert_eq!(
            bare().with_project_id(&overlong),
            Err(TraceLinkError::InvalidProjectId)
        );
        // Charset guards: no path separators or whitespace that could smuggle a
        // different path or a credential.
        for hostile in ["has/slash", "has space", "query?x", "user@host"] {
            assert_eq!(
                bare().with_project_id(hostile),
                Err(TraceLinkError::InvalidProjectId),
                "must reject project id {hostile:?}"
            );
        }
        assert!(bare().with_project_id(PROJECT_ID).is_ok());
    }

    #[test]
    fn default_host_is_the_documented_origin() {
        assert_eq!(
            LangfuseTraceLink::default(),
            LangfuseTraceLink::new(DEFAULT_LANGFUSE_HOST).unwrap()
        );
    }

    // ---- Absent / malformed ids never produce a link ----

    #[test]
    fn absent_trace_id_produces_no_link() {
        let link = LangfuseTraceLink::default();
        assert_eq!(
            link.deep_link(&retained(None)),
            None,
            "no exported OTel trace id must yield no link, not a malformed one"
        );
    }

    #[test]
    fn malformed_trace_ids_produce_no_link() {
        let link = LangfuseTraceLink::default();
        for bad in [
            "",
            "4BF92F3577B34DA6A3CE929D0E0E4736", // uppercase
            "4bf92f3577b34da6a3ce929d0e0e473",  // 31 hex (too short)
            "4bf92f3577b34da6a3ce929d0e0e47360", // 33 hex (too long)
            "4bf92f3577b34da6a3ce929d0e0e473g",  // non-hex char
            "4bf92f3577b34da6 a3ce929d0e0e4736", // embedded space
            "00000000000000000000000000000000",  // all-zero (invalid per spec)
        ] {
            assert_eq!(
                link.trace_url_from_hex(bad),
                None,
                "must never link from {bad:?}"
            );
        }
    }

    #[test]
    fn blank_request_correlation_produces_no_link() {
        let link = LangfuseTraceLink::default();
        for blank in ["", "   ", "\t"] {
            let trace = RetainedTrace::new(blank, Some(TRACE_ID_HEX), None);
            assert_eq!(link.deep_link(&trace), None, "blank {blank:?} must not link");
        }
    }

    #[test]
    fn all_zero_otel_ids_are_dropped_and_not_linked() {
        let link = LangfuseTraceLink::default();
        // Direct value path: an invalid TraceId renders to all-zeros → None.
        assert_eq!(link.trace_url(TraceId::INVALID), None);
        assert_eq!(
            link.trace_url(TraceId::from_hex("00000000000000000000000000000000").unwrap()),
            None
        );
        // Identity path: an invalid TraceId/SpanId is dropped to None, so the
        // deep link sees "no exported trace id" → None.
        let trace = RetainedTrace::from_otel(REQUEST_TRACE_ID, Some(TraceId::INVALID), None);
        assert_eq!(trace.otel_trace_id, None);
        assert_eq!(link.deep_link(&trace), None);

        let trace = RetainedTrace::from_otel(
            REQUEST_TRACE_ID,
            Some(TraceId::from_hex(TRACE_ID_HEX).unwrap()),
            Some(SpanId::INVALID),
        );
        assert_eq!(trace.otel_span_id, None, "all-zero span id dropped");
    }
}
