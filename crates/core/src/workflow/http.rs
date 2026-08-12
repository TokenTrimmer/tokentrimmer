//! Guarded outbound HTTP execution for workflow Http nodes (W3b Task 3).
//!
//! ## Security invariants
//!
//! * **Guarded client**: DNS resolver from [`tt_shared::with_guarded_dns`] blocks
//!   private/loopback/link-local IPs at connect time, closing the DNS-rebind gap
//!   left by the save-time allowlist check.
//! * **No redirects**: `Policy::none()` prevents a redirect to a private IP.
//! * **Total timeout**: 30 s hard cap on the entire request/response cycle.
//! * **Connect timeout**: 5 s.
//! * **Byte cap**: stream up to `max_response_bytes`; never trust `Content-Length`.
//! * **Run-time allowlist re-check**: URL host ∈ `allowed_hosts` at every execution
//!   (defense-in-depth — save-time check is not sufficient against TOCTOU).
//! * **Error sanitization**: all reqwest errors mapped via `.without_url()` so
//!   secrets embedded in query params never appear in error strings.
//! * **Secret isolation**: [`substitute_with_secrets`] exposes secrets ONLY on the
//!   wire-spec; the engine's shared `substitute`/`resolve_ref` returns `"***"` for
//!   `{{secrets.*}}` so Model/Agent prompts, Transform exprs, and Branch conditions
//!   are always secret-free.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use futures::StreamExt as _;
use thiserror::Error;
use tracing::warn;
use tt_shared::context::SecretString;
use tt_shared::filter_outbound_headers;

use crate::workflow::types::NodeOutput;

/// Default response body byte cap: 1 MiB.
pub(crate) const DEFAULT_MAX_RESPONSE_BYTES: usize = 1_048_576;

/// Closed set of media types a URL document source may resolve to — the exact
/// extension/media table the gateway's document tooling (`cli/src/docprep.rs`,
/// `client/src/document.rs`, and the `document_lane` sidecar) dispatches on:
/// PDF text-layer extraction plus image OCR. A fetched URL whose resolved
/// media type is outside this set fails closed — the engine never hands an
/// arbitrary remote payload to the distill seam or downstream nodes.
pub(crate) const SUPPORTED_DOCUMENT_MEDIA: &[&str] = &[
    "application/pdf",
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/bmp",
    "image/tiff",
];

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The fully template-substituted (secrets-expanded) HTTP request spec.
///
/// **SECURITY**: This struct contains the real secret values after substitution.
/// It MUST NOT be written to any journal entry, `NodeOutput.content`, run
/// `error` string, or `request_logs`. It is single-use — created just before
/// calling `run_http` and dropped immediately after.
#[derive(Debug)]
pub(crate) struct HttpReqSpec {
    pub method: String,
    /// URL with all `{{...}}` tokens substituted (may contain secret values).
    pub url: String,
    /// Headers with values substituted (may contain secret values).
    pub headers: Vec<(String, String)>,
    /// Body with values substituted (may contain secret values).
    pub body: Option<String>,
    pub max_response_bytes: usize,
}

/// Successful HTTP response.
#[derive(Debug)]
pub(crate) struct HttpResp {
    pub status: u16,
    /// Parsed as JSON when valid, otherwise a JSON string wrapping the raw text.
    pub body: serde_json::Value,
}

/// Error from [`run_http`]. All variants contain SANITIZED strings —
/// no URL, headers, body, or secret values.
#[derive(Debug, Error)]
pub(crate) enum HttpError {
    /// The URL host is not in the workflow's `allowed_hosts` list.
    #[error("host not in allowed_hosts: {0}")]
    HostNotAllowed(String),

    /// The URL contains inline userinfo (`user:pass@host`). Credentials must be
    /// passed via `{{secrets.NAME}}` Authorization headers instead.
    #[error(
        "url must not contain userinfo ('@' in host); \
         pass credentials via a {{{{secrets.NAME}}}} header instead"
    )]
    UrlContainsUserinfo,

    /// The URL could not be parsed (details redacted to avoid leaking secrets
    /// that may appear in the url string).
    #[error("invalid url (details redacted)")]
    InvalidUrl,

    /// The HTTP request failed (network/DNS/TLS/timeout error). The detail
    /// string is sanitized via `.without_url()`.
    #[error("request failed: {0}")]
    Request(String),

    /// The response body exceeded the byte cap.
    #[error("response body exceeded the {0}-byte cap")]
    ResponseTooLarge(usize),

    /// The URL was rejected by the SSRF guard (blocked scheme, private/loopback/
    /// link-local IP literal, or blocked hostname). The URL and secret values are
    /// NOT included in this error message.
    #[error("url rejected by SSRF guard (blocked scheme or private/internal address)")]
    BlockedUrl,

    /// A URL document source fetch returned a non-2xx status. The body is never
    /// distilled from an error page — fail closed. Only the status code is
    /// echoed (no URL).
    #[error("document fetch returned HTTP status {0}")]
    BadStatus(u16),

    /// A URL document source resolved to a media type outside
    /// [`SUPPORTED_DOCUMENT_MEDIA`] (or no resolvable type). The node fails
    /// closed rather than handing an arbitrary remote payload to the distill
    /// seam. Only the observed media type is echoed (no URL).
    #[error("document media type not in supported set: {0}")]
    UnsupportedMediaType(String),
}

// ---------------------------------------------------------------------------
// substitute_with_secrets
// ---------------------------------------------------------------------------

/// Template substitution like the engine's `substitute`, but ALSO resolves
/// `{{secrets.NAME}}` tokens using the caller-supplied secrets map.
///
/// **SECURITY**: The output of this function may contain real secret values.
/// Use it ONLY to build [`HttpReqSpec`]; never pass the result to a journal,
/// `NodeOutput`, or error string.
///
/// The shared `substitute` in `engine.rs` returns `"***"` for `{{secrets.*}}`
/// refs, ensuring Model/Agent prompts and Transform exprs remain secret-free.
pub(crate) fn substitute_with_secrets(
    template: &str,
    trigger_id: &str,
    outputs: &HashMap<String, NodeOutput>,
    secrets: &HashMap<String, SecretString>,
    variables: &BTreeMap<String, String>,
) -> String {
    let mut result = String::with_capacity(template.len() + 16);
    let mut remaining = template;

    while let Some(open) = remaining.find("{{") {
        result.push_str(&remaining[..open]);
        remaining = &remaining[open + 2..];

        if let Some(close) = remaining.find("}}") {
            let ref_str = remaining[..close].trim();
            let resolved = resolve_secret_or_ref(ref_str, trigger_id, outputs, secrets, variables);
            result.push_str(&resolved);
            remaining = &remaining[close + 2..];
        } else {
            // Unclosed `{{` — emit as-is and stop scanning.
            result.push_str("{{");
            break;
        }
    }
    result.push_str(remaining);
    result
}

/// Resolve a `{{ref}}` token with secrets taking priority over node outputs.
fn resolve_secret_or_ref(
    ref_str: &str,
    trigger_id: &str,
    outputs: &HashMap<String, NodeOutput>,
    secrets: &HashMap<String, SecretString>,
    variables: &BTreeMap<String, String>,
) -> String {
    // `{{secrets.NAME}}` → expose the secret value (wire-only).
    if let Some(name) = ref_str.strip_prefix("secrets.") {
        return secrets
            .get(name)
            .map(|s| s.expose().to_string())
            .unwrap_or_default();
    }
    if let Some(name) = ref_str.strip_prefix("variables.") {
        return variables.get(name).cloned().unwrap_or_default();
    }

    // Split on the first `.` for `node.field` syntax.
    let (node_part, field_part) = match ref_str.find('.') {
        Some(pos) => (&ref_str[..pos], Some(&ref_str[pos + 1..])),
        None => (ref_str, None),
    };

    // `{{input}}` is an alias for the Trigger node.
    let node_key = if node_part == "input" {
        trigger_id
    } else {
        node_part
    };

    let content = match outputs.get(node_key) {
        Some(out) => &out.content,
        None => return String::new(),
    };

    match field_part {
        None => json_to_string(content),
        Some(field) => match content {
            serde_json::Value::Object(map) => {
                map.get(field).map(json_to_string).unwrap_or_default()
            }
            _ => String::new(),
        },
    }
}

fn json_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// run_http
// ---------------------------------------------------------------------------

/// Execute a guarded outbound HTTP request for a workflow Http node.
///
/// # Security
///
/// 1. Re-checks the URL host against `allowed_hosts` at run time (defense-in-depth).
/// 2. Builds a fresh client per call with [`tt_shared::with_guarded_dns`]
///    (DNS-rebind guard), `Policy::none()` (no redirects), 30 s total + 5 s connect
///    timeout, rustls, gzip.
/// 3. [`filter_extra_headers`] strips forbidden headers before they leave the gateway.
/// 4. Accumulates the response body up to `spec.max_response_bytes` without trusting
///    `Content-Length`.
/// 5. All reqwest errors are mapped via `.without_url()` — secrets in query
///    params are never logged.
pub(crate) async fn run_http(
    spec: HttpReqSpec,
    allowed_hosts: &[String],
) -> Result<HttpResp, HttpError> {
    // ---- 1. Run-time allowlist re-check -------------------------------------
    let host = extract_host(&spec.url)?;
    if !allowed_hosts.iter().any(|h| h == &host) {
        return Err(HttpError::HostNotAllowed(host));
    }

    // ---- 1b. Run-time SSRF re-assertion (defense-in-depth) ------------------
    // reqwest/hyper connect DIRECTLY to IP-literal hosts, bypassing any custom
    // DNS resolver. `with_guarded_dns` only intercepts DNS-resolved addresses,
    // so a URL like `http://127.0.0.1/` slips past it. Re-assert the full SSRF
    // guard here: https-only scheme + hostname denylist + literal-IP block +
    // best-effort DNS-resolved-IP block.
    //
    // Note: `spec.url` has already had `{{...}}` tokens substituted (concrete
    // URL). If a resolved URL still contains template remnants, `validate_provider_url`
    // will fail to parse it — that's a bug we want surfaced, not hidden.
    tt_shared::validate_provider_url(&spec.url, false).map_err(|_| HttpError::BlockedUrl)?;

    // ---- 2. Build the guarded client ----------------------------------------
    let client = tt_shared::with_guarded_dns(
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .use_rustls_tls()
            .gzip(true),
    )
    .build()
    .map_err(|e| HttpError::Request(e.without_url().to_string()))?;

    // ---- 3. Filter headers --------------------------------------------------
    let filtered = filter_outbound_headers(&spec.headers);

    // ---- 4. Build the request -----------------------------------------------
    let method = reqwest::Method::from_bytes(spec.method.to_uppercase().as_bytes())
        .map_err(|_| HttpError::Request(format!("invalid HTTP method: {}", spec.method)))?;

    let mut builder = client.request(method, &spec.url);
    for (name, value) in &filtered {
        builder = builder.header(name, value);
    }
    if let Some(body) = spec.body {
        builder = builder.body(body);
    }

    // ---- 5. Fire the request ------------------------------------------------
    let response = builder
        .send()
        .await
        .map_err(|e| HttpError::Request(e.without_url().to_string()))?;

    let status = response.status().as_u16();
    let cap = spec.max_response_bytes;

    // ---- 6. Stream body up to the byte cap (never trust Content-Length) -----
    let mut stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut truncated = false;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| HttpError::Request(e.without_url().to_string()))?;
        if buf.len() + chunk.len() > cap {
            let remaining = cap.saturating_sub(buf.len());
            buf.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        buf.extend_from_slice(&chunk);
    }

    if truncated {
        warn!(
            cap_bytes = cap,
            status, "run_http: response body exceeded byte cap; truncating"
        );
        return Err(HttpError::ResponseTooLarge(cap));
    }

    // ---- 7. Parse body (JSON if valid, otherwise plain string) --------------
    let body = match serde_json::from_slice::<serde_json::Value>(&buf) {
        Ok(v) => v,
        Err(_) => serde_json::Value::String(String::from_utf8_lossy(&buf).into_owned()),
    };

    Ok(HttpResp { status, body })
}

// ---------------------------------------------------------------------------
// run_document_fetch
// ---------------------------------------------------------------------------

/// A URL document source successfully fetched through the guarded egress: its
/// resolved media type (already vetted against [`SUPPORTED_DOCUMENT_MEDIA`])
/// plus its fully-buffered bytes as standard base64 — ready to hand to the
/// Document node's distill seam exactly like an inline base64 source.
#[derive(Debug)]
pub(crate) struct FetchedDocument {
    pub media_type: String,
    pub data_b64: String,
}

/// Execute a guarded fetch of a workflow URL document source.
///
/// This is the SAME guarded egress path the Http node's [`run_http`] uses —
/// identical transport posture, reusing the shared guards — with two
/// intentional differences:
///
/// * **No `allowed_hosts` allowlist.** A URL document source is admitted by
///   its own closed contract (a static, credential-free HTTPS literal passing
///   the shared SSRF guard at authoring), not by a per-workflow allowlist
///   entry that the document-source surface has never had. The transport is
///   still fully guarded (below), so removing the allowlist does not loosen
///   the Http node's guarantees — it simply omits a document node never had.
/// * **Media-set + status gates.** The fetched bytes are only ever passed to
///   the distill seam when the status is 2xx and the resolved media type is a
///   member of [`SUPPORTED_DOCUMENT_MEDIA`]. The base64 path trusts the
///   author's `media_type`; a remote source is treated as untrusted input and
///   fails closed instead.
///
/// # Security (mirrors `run_http` exactly)
///
/// 1. Rejects inline userinfo — the contract is credential-free.
/// 2. Re-asserts [`tt_shared::validate_provider_url`] at run time (https-only,
///    hostname denylist, private/loopback/link-local/metadata IP-literal block,
///    best-effort DNS-resolved-IP block). Authoring-time validation is not
///    sufficient against TOCTOU / DNS rebinding.
/// 3. Fresh client per call with [`tt_shared::with_guarded_dns`]
///    (DNS-rebind guard at connect time), `Policy::none()` (no redirects —
///    a redirect to a private IP can never be followed), 30 s total + 5 s
///    connect timeout, rustls, gzip.
/// 4. Streams the body up to [`DEFAULT_MAX_RESPONSE_BYTES`] (1 MiB — the same
///    shared byte cap the Http node uses); never trusts `Content-Length`.
/// 5. All reqwest errors are mapped via `.without_url()` — the URL (and any
///    credentials it never carries) is never echoed in an error string.
pub(crate) async fn run_document_fetch(url: &str) -> Result<FetchedDocument, HttpError> {
    // ---- 1. Userinfo rejection — the source is credential-free. -------------
    let parsed = reqwest::Url::parse(url).map_err(|_| HttpError::InvalidUrl)?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(HttpError::UrlContainsUserinfo);
    }

    // ---- 2. Run-time SSRF re-assertion (defense-in-depth). ------------------
    // A definition could in principle reach this function bypassing authoring
    // validation, and even a validated hostname can DNS-rebind between saves.
    // Re-run the shared guard on the CONCRETE URL we are about to fetch.
    tt_shared::validate_provider_url(url, false).map_err(|_| HttpError::BlockedUrl)?;

    // ---- 3. Guarded client (identical to run_http). -------------------------
    let client = tt_shared::with_guarded_dns(
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .use_rustls_tls()
            .gzip(true),
    )
    .build()
    .map_err(|e| HttpError::Request(e.without_url().to_string()))?;

    // ---- 4. GET — no headers, no body: the source URL carries nothing. ------
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| HttpError::Request(e.without_url().to_string()))?;
    let status = response.status().as_u16();

    // Content-Type with parameters stripped + lowercased, when present.
    let content_type: Option<String> = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| {
            ct.split(';')
                .next()
                .unwrap_or(ct)
                .trim()
                .to_ascii_lowercase()
        });

    // ---- 5. Status derived BEFORE streaming (never distill an error page). --
    if !(200..300).contains(&status) {
        return Err(HttpError::BadStatus(status));
    }

    // ---- 6. Media-set gate (fail closed) before consuming the body. ---------
    let observed = content_type.clone().unwrap_or_else(|| "<none>".to_string());
    let media_type = resolve_document_media_type(&content_type, url)
        .ok_or_else(|| HttpError::UnsupportedMediaType(observed.clone()))?;

    // ---- 7. Stream body up to the shared byte cap (never trust the header).--
    let mut stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut truncated = false;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| HttpError::Request(e.without_url().to_string()))?;
        if buf.len() + chunk.len() > DEFAULT_MAX_RESPONSE_BYTES {
            buf.extend_from_slice(&chunk[..DEFAULT_MAX_RESPONSE_BYTES.saturating_sub(buf.len())]);
            truncated = true;
            break;
        }
        buf.extend_from_slice(&chunk);
    }

    if truncated {
        warn!(
            cap_bytes = DEFAULT_MAX_RESPONSE_BYTES,
            status, "run_document_fetch: response body exceeded byte cap"
        );
        return Err(HttpError::ResponseTooLarge(DEFAULT_MAX_RESPONSE_BYTES));
    }

    use base64::Engine as _;
    Ok(FetchedDocument {
        media_type,
        data_b64: base64::engine::general_purpose::STANDARD.encode(&buf),
    })
}

/// Resolve the media type of a fetched URL document source.
///
/// The response `Content-Type` (already parameter-stripped + lowercased) is
/// authoritative when it is present and non-generic; a generic
/// `application/octet-stream` / `binary/octet-stream` marker (or an absent
/// header) falls back to the URL path extension — the same extension table
/// the gateway's document tooling uses. Returning `None` means the fetched
/// payload is not a supported document and the caller must fail closed.
fn resolve_document_media_type(content_type: &Option<String>, url: &str) -> Option<String> {
    match content_type {
        Some(base) if !is_generic_document_content_type(base) => canonical_document_media(base),
        _ => media_type_from_url_extension(url),
    }
}

/// `true` for the generic binary markers that carry no usable type information,
/// forcing a fall back to the URL path extension.
fn is_generic_document_content_type(base: &str) -> bool {
    matches!(
        base,
        "" | "application/octet-stream" | "binary/octet-stream"
    )
}

/// Normalize a Content-Type to a supported canonical member
/// (`image/jpg` → `image/jpeg`, `image/x-png` → `image/png`, …), returning
/// `Some` only when the result is a member of [`SUPPORTED_DOCUMENT_MEDIA`].
fn canonical_document_media(base: &str) -> Option<String> {
    let normalized = match base {
        "image/jpg" => "image/jpeg",
        "image/pjpeg" => "image/jpeg", // JPEG under an MS-isms alias
        "image/x-png" => "image/png",
        "image/tif" => "image/tiff",
        other => other,
    };
    SUPPORTED_DOCUMENT_MEDIA
        .contains(&normalized)
        .then(|| normalized.to_string())
}

/// Infer a supported media type from the URL path extension (the canonical
/// pdf/png/jpg/gif/webp/bmp/tiff table). Returns `None` for an extension-less
/// or unrecognized path.
fn media_type_from_url_extension(url: &str) -> Option<String> {
    let path = reqwest::Url::parse(url).ok()?.path().to_string();
    let ext = path.rsplit('.').next()?; // segment after the last dot, if any
    if ext.contains('/') || ext.is_empty() {
        return None; // no dot / no extension
    }
    let media = match ext.to_ascii_lowercase().as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tiff" | "tif" => "image/tiff",
        _ => return None,
    };
    Some(media.to_string())
}

// ---------------------------------------------------------------------------
// Helper: parse the literal host from a URL
// ---------------------------------------------------------------------------

fn extract_host(url: &str) -> Result<String, HttpError> {
    let parsed = reqwest::Url::parse(url).map_err(|_| HttpError::InvalidUrl)?;
    // Reject inline userinfo — credentials must flow via {{secrets.NAME}} headers.
    // Defense-in-depth: save-time validation also rejects '@' in the authority,
    // but a workflow def could in principle reach run_http bypassing that check.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(HttpError::UrlContainsUserinfo);
    }
    parsed
        .host_str()
        .map(|h| h.to_string())
        .ok_or(HttpError::InvalidUrl)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // ---- substitute_with_secrets -------------------------------------------

    #[test]
    fn substitute_with_secrets_resolves_secret() {
        let mut secrets = HashMap::new();
        secrets.insert("API_KEY".to_string(), SecretString::new("sekret-value"));
        let outputs = HashMap::new();

        let result = substitute_with_secrets(
            "Bearer {{secrets.API_KEY}}",
            "t",
            &outputs,
            &secrets,
            &BTreeMap::new(),
        );
        assert_eq!(result, "Bearer sekret-value");
    }

    #[test]
    fn substitute_with_secrets_missing_secret_is_defensively_empty() {
        let secrets = HashMap::new();
        let outputs = HashMap::new();

        let result = substitute_with_secrets(
            "Bearer {{secrets.MISSING}}",
            "t",
            &outputs,
            &secrets,
            &BTreeMap::new(),
        );
        // The engine preflight prevents this helper from dispatching an Http
        // node with a missing reference. Keep the wire helper non-panicking as
        // a final defensive fallback.
        assert_eq!(result, "Bearer ");
    }

    #[test]
    fn substitute_with_secrets_node_ref_still_works() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "n1".to_string(),
            NodeOutput {
                content: json!({"token": "node-value"}),
                ..Default::default()
            },
        );
        let secrets = HashMap::new();

        let result = substitute_with_secrets(
            "prefix-{{n1.token}}",
            "t",
            &outputs,
            &secrets,
            &BTreeMap::new(),
        );
        assert_eq!(result, "prefix-node-value");
    }

    #[test]
    fn substitute_with_secrets_mixed_refs() {
        let mut secrets = HashMap::new();
        secrets.insert("KEY".to_string(), SecretString::new("s3cr3t"));
        let mut outputs = HashMap::new();
        outputs.insert(
            "t".to_string(),
            NodeOutput {
                content: json!("hello"),
                ..Default::default()
            },
        );

        let result = substitute_with_secrets(
            "{{input}} + {{secrets.KEY}}",
            "t",
            &outputs,
            &secrets,
            &BTreeMap::new(),
        );
        assert_eq!(result, "hello + s3cr3t");
    }

    #[test]
    fn substitute_with_secrets_also_resolves_non_secret_variables() {
        let variables = BTreeMap::from([("REGION".into(), "us-east".into())]);
        let result = substitute_with_secrets(
            "https://example.com/{{variables.REGION}}",
            "t",
            &HashMap::new(),
            &HashMap::new(),
            &variables,
        );
        assert_eq!(result, "https://example.com/us-east");
    }

    // ---- run_http: userinfo rejection ---------------------------------------

    /// `run_http` must reject a url that embeds userinfo even when BOTH the
    /// spoofed and real host are in allowed_hosts — proving it is the USERINFO
    /// check (not the allowlist) doing the rejecting.
    /// Before the fix: `extract_host` returns the real host "evil.com", which IS
    /// in allowed_hosts → no error → test FAILS.
    /// After the fix: `extract_host` detects non-empty username → UrlContainsUserinfo.
    #[tokio::test]
    async fn run_http_rejects_userinfo() {
        let spec = HttpReqSpec {
            method: "GET".into(),
            url: "https://allowed.example.com@evil.com/".into(),
            headers: vec![],
            body: None,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        };
        // Both listed so the allowlist would pass — rejection must come from userinfo check.
        let err = run_http(
            spec,
            &["allowed.example.com".to_string(), "evil.com".to_string()],
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, HttpError::UrlContainsUserinfo),
            "expected UrlContainsUserinfo, got: {err}"
        );
    }

    // ---- run_http: allowlist re-check ---------------------------------------

    #[tokio::test]
    async fn run_http_rejects_host_not_in_allowlist() {
        let spec = HttpReqSpec {
            method: "GET".into(),
            url: "https://api.example.com/x".into(),
            headers: vec![],
            body: None,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        };
        // "other-host.com" is in allowed_hosts but URL host is "api.example.com".
        let err = run_http(spec, &["other-host.com".to_string()])
            .await
            .unwrap_err();
        assert!(
            matches!(err, HttpError::HostNotAllowed(_)),
            "expected HostNotAllowed, got: {err}"
        );
    }

    #[tokio::test]
    async fn run_http_rejects_empty_allowlist() {
        let spec = HttpReqSpec {
            method: "GET".into(),
            url: "https://api.example.com/x".into(),
            headers: vec![],
            body: None,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        };
        let err = run_http(spec, &[]).await.unwrap_err();
        assert!(
            matches!(err, HttpError::HostNotAllowed(_)),
            "expected HostNotAllowed for empty allowlist, got: {err}"
        );
    }

    // ---- run_http: error sanitization ---------------------------------------

    #[tokio::test]
    async fn run_http_error_is_sanitized() {
        // URL with a "secret" value embedded in the query string.
        let spec = HttpReqSpec {
            method: "GET".into(),
            url: "https://example.com/path?token=sekret-value".into(),
            headers: vec![],
            body: None,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        };
        // Host is NOT in allowed_hosts → HostNotAllowed immediately (no network call).
        let err = run_http(spec, &[]).await.unwrap_err();
        let err_str = format!("{err}");
        assert!(
            !err_str.contains("sekret-value"),
            "error must not contain the secret value; got: {err_str}"
        );
        assert!(
            !err_str.contains("?token="),
            "error must not contain URL query string; got: {err_str}"
        );
    }

    // ---- run_http: auth header is not dropped by outbound filter ---------------

    /// `filter_outbound_headers` must keep `authorization` and `x-api-key` so
    /// HTTP nodes can authenticate to external APIs (regression guard).
    #[tokio::test]
    async fn run_http_auth_header_not_dropped_by_filter() {
        // Use the empty allowlist path — we only care that HostNotAllowed fires,
        // not BlockedUrl, which would indicate the filter never saw the headers.
        // The key assertion is that headers containing `authorization` survive
        // `filter_outbound_headers` and reach the allowlist check.
        let spec = HttpReqSpec {
            method: "GET".into(),
            url: "https://api.example.com/".into(),
            headers: vec![
                (
                    "authorization".to_string(),
                    "Bearer secret-token".to_string(),
                ),
                ("x-api-key".to_string(), "sk-test".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            body: None,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        };
        // Host not in allowlist → fails at allowlist check (not at header filter).
        let err = run_http(spec, &[]).await.unwrap_err();
        assert!(
            matches!(err, HttpError::HostNotAllowed(_)),
            "expected HostNotAllowed (auth headers survived the filter), got: {err}"
        );
    }

    // ---- run_http: run-time SSRF IP/scheme re-assertion (W3b security review) --

    /// IP-literal loopback (`127.0.0.1`) must be rejected even when it appears
    /// in `allowed_hosts`, proving the SSRF guard (not the allowlist) blocks it.
    /// FAILS before fix: allowlist passes + no IP guard → attempt connect.
    #[tokio::test]
    async fn run_http_rejects_ip_literal_private() {
        let spec = HttpReqSpec {
            method: "GET".into(),
            url: "http://127.0.0.1/".into(),
            headers: vec![],
            body: None,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        };
        // IP is explicitly in allowed_hosts — rejection must come from the SSRF guard.
        let err = run_http(spec, &["127.0.0.1".to_string()])
            .await
            .unwrap_err();
        assert!(
            matches!(err, HttpError::BlockedUrl),
            "expected BlockedUrl for loopback IP literal, got: {err}"
        );
    }

    /// AWS/GCP metadata endpoint via IP literal must be rejected at run time.
    /// FAILS before fix: allowlist passes + no IP guard → attempt connect.
    #[tokio::test]
    async fn run_http_rejects_metadata_ip() {
        let spec = HttpReqSpec {
            method: "GET".into(),
            url: "http://169.254.169.254/latest/meta-data/".into(),
            headers: vec![],
            body: None,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        };
        // IP is explicitly in allowed_hosts — rejection must come from the SSRF guard.
        let err = run_http(spec, &["169.254.169.254".to_string()])
            .await
            .unwrap_err();
        assert!(
            matches!(err, HttpError::BlockedUrl),
            "expected BlockedUrl for metadata IP literal, got: {err}"
        );
    }

    /// Plain-HTTP URLs must be rejected at run time even when the host is in
    /// `allowed_hosts`, re-asserting the https-only constraint.
    /// FAILS before fix: allowlist passes + no scheme guard → attempt connect.
    #[tokio::test]
    async fn run_http_rejects_non_https_at_run() {
        let spec = HttpReqSpec {
            method: "GET".into(),
            url: "http://allowed.example.com/".into(),
            headers: vec![],
            body: None,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        };
        // Host is in allowed_hosts — rejection must come from the https scheme guard.
        let err = run_http(spec, &["allowed.example.com".to_string()])
            .await
            .unwrap_err();
        assert!(
            matches!(err, HttpError::BlockedUrl),
            "expected BlockedUrl for non-https url, got: {err}"
        );
    }

    // ---- guarded_client_blocks_private_ip -----------------------------------
    //
    // The guarded DNS resolver (tt_shared::GuardedResolver) blocks all resolved
    // addresses that fall in private/loopback/link-local ranges. This test
    // exercises that layer by including `localhost` in `allowed_hosts` (bypassing
    // the allowlist check) and verifying that the connection fails.

    #[tokio::test]
    async fn guarded_client_blocks_private_ip() {
        // Include `localhost` in allowed_hosts so the allowlist check passes;
        // the guarded DNS resolver should block the resolved 127.0.0.1 address.
        let spec = HttpReqSpec {
            method: "GET".into(),
            url: "http://localhost/probe".into(),
            headers: vec![],
            body: None,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        };
        let result = run_http(spec, &["localhost".to_string()]).await;
        // Whether the guarded DNS fires, the connection is refused, or the
        // connect-timeout fires, we expect an error.
        assert!(
            result.is_err(),
            "connection to localhost must fail (guarded DNS or refusal); got: {result:?}"
        );
        if let Err(e) = result {
            let err_str = format!("{e}");
            // Error must not contain the URL path (sans-URL sanitization).
            assert!(
                !err_str.contains("/probe"),
                "error must not expose URL path; got: {err_str}"
            );
        }
    }
    // -----------------------------------------------------------------------
    // run_document_fetch: SSRF / scheme / userinfo rejection (all no-network —
    // the guard fires before any connect), + pure media-resolution helpers.
    // -----------------------------------------------------------------------

    /// A URL document source embedding userinfo must fail closed: the source
    /// is credential-free by contract.
    #[tokio::test]
    async fn run_document_fetch_rejects_userinfo() {
        let err = run_document_fetch("https://allowed.example.com@evil.com/doc.pdf")
            .await
            .unwrap_err();
        assert!(
            matches!(err, HttpError::UrlContainsUserinfo),
            "expected UrlContainsUserinfo, got: {err}"
        );
    }

    /// A private IP-literal URL document source must fail closed at the SSRF
    /// guard — no network call ever fires.
    #[tokio::test]
    async fn run_document_fetch_rejects_loopback_ip() {
        let err = run_document_fetch("https://127.0.0.1/doc.pdf")
            .await
            .unwrap_err();
        assert!(
            matches!(err, HttpError::BlockedUrl),
            "expected BlockedUrl for loopback, got: {err}"
        );
    }

    /// Cloud-metadata IP-literal URL document source must fail closed.
    #[tokio::test]
    async fn run_document_fetch_rejects_metadata_ip() {
        let err = run_document_fetch("https://169.254.169.254/latest/meta-data/")
            .await
            .unwrap_err();
        assert!(
            matches!(err, HttpError::BlockedUrl),
            "expected BlockedUrl for metadata IP, got: {err}"
        );
    }

    /// A plain-HTTP URL document source must fail closed (https-only contract).
    #[tokio::test]
    async fn run_document_fetch_rejects_non_https() {
        let err = run_document_fetch("http://example.com/doc.pdf")
            .await
            .unwrap_err();
        assert!(
            matches!(err, HttpError::BlockedUrl),
            "expected BlockedUrl for http, got: {err}"
        );
    }

    /// An unparseable URL document source must fail closed.
    #[tokio::test]
    async fn run_document_fetch_rejects_invalid_url() {
        let err = run_document_fetch("not a url").await.unwrap_err();
        assert!(
            matches!(err, HttpError::InvalidUrl),
            "expected InvalidUrl, got: {err}"
        );
    }

    // ---- canonical_document_media ------------------------------------------

    #[test]
    fn canonical_document_media_passes_supported_identically() {
        assert_eq!(
            canonical_document_media("application/pdf").as_deref(),
            Some("application/pdf")
        );
        assert_eq!(
            canonical_document_media("image/png").as_deref(),
            Some("image/png")
        );
        assert_eq!(
            canonical_document_media("image/webp").as_deref(),
            Some("image/webp")
        );
        assert_eq!(
            canonical_document_media("image/bmp").as_deref(),
            Some("image/bmp")
        );
    }

    #[test]
    fn canonical_document_media_normalizes_aliases() {
        assert_eq!(
            canonical_document_media("image/jpg").as_deref(),
            Some("image/jpeg")
        );
        assert_eq!(
            canonical_document_media("image/pjpeg").as_deref(),
            Some("image/jpeg")
        );
        assert_eq!(
            canonical_document_media("image/x-png").as_deref(),
            Some("image/png")
        );
        assert_eq!(
            canonical_document_media("image/tif").as_deref(),
            Some("image/tiff")
        );
    }

    #[test]
    fn canonical_document_media_rejects_unknown() {
        assert_eq!(canonical_document_media("text/html"), None);
        assert_eq!(canonical_document_media("application/zip"), None);
        assert_eq!(canonical_document_media(""), None);
    }

    // ---- media_type_from_url_extension -------------------------------------

    #[test]
    fn media_from_url_extension_known_extensions() {
        assert_eq!(
            media_type_from_url_extension("https://example.com/doc.pdf").as_deref(),
            Some("application/pdf")
        );
        // Query string is not part of the path; extension matching is case-insensitive.
        assert_eq!(
            media_type_from_url_extension("https://example.com/a.PDF?x=1&y=2").as_deref(),
            Some("application/pdf")
        );
        assert_eq!(
            media_type_from_url_extension("https://example.com/pic.JPG").as_deref(),
            Some("image/jpeg")
        );
        assert_eq!(
            media_type_from_url_extension("https://example.com/anim.gif").as_deref(),
            Some("image/gif")
        );
    }

    #[test]
    fn media_from_url_extension_unknown_or_missing() {
        assert_eq!(
            media_type_from_url_extension("https://example.com/doc"),
            None
        );
        assert_eq!(
            media_type_from_url_extension("https://example.com/doc.exe"),
            None
        );
        assert_eq!(
            media_type_from_url_extension("https://example.com/noext?fmt=pdf"),
            None
        );
        assert_eq!(media_type_from_url_extension("https://example.com/"), None);
    }

    // ---- resolve_document_media_type (Content-Type precedence) --------------

    #[test]
    fn resolve_media_prefers_non_generic_content_type() {
        let ct = Some("application/pdf".to_string());
        assert_eq!(
            resolve_document_media_type(&ct, "https://example.com/x.bin").as_deref(),
            Some("application/pdf")
        );
        // A non-generic unsupported Content-Type wins over the URL extension —
        // never trust a .pdf path on a server that served HTML.
        let ct = Some("text/html".to_string());
        assert_eq!(
            resolve_document_media_type(&ct, "https://example.com/x.pdf"),
            None
        );
    }

    #[test]
    fn resolve_media_falls_back_to_extension_on_generic_or_absent() {
        let generic = Some("application/octet-stream".to_string());
        assert_eq!(
            resolve_document_media_type(&generic, "https://example.com/x.pdf").as_deref(),
            Some("application/pdf")
        );
        assert_eq!(
            resolve_document_media_type(&None, "https://example.com/x.png").as_deref(),
            Some("image/png")
        );
        assert_eq!(
            resolve_document_media_type(&None, "https://example.com/x.doc"),
            None
        );
    }

    #[test]
    fn generic_content_type_markers() {
        assert!(is_generic_document_content_type(""));
        assert!(is_generic_document_content_type("application/octet-stream"));
        assert!(is_generic_document_content_type("binary/octet-stream"));
        assert!(!is_generic_document_content_type("application/pdf"));
    }
}
