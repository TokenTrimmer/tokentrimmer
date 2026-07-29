//! OpenAI-compatible request/response shapes. Canonical wire format across all
//! providers — adapters translate to/from provider-native formats.

use std::collections::HashMap;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::Usage;

// ---------------------------------------------------------------------------
// tt_extras cache-control types (Fix B / §2.7)
// ---------------------------------------------------------------------------

/// Cache behaviour requested by the caller via `tt_extras.cache`.
///
/// Absent (no `cache` key in `tt_extras`) is treated as [`CacheMode::Normal`].
///
/// JSON shape:
/// ```json
/// { "cache": { "mode": "bypass", "ttl_secs": 3600 } }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheMode {
    /// Normal read-write caching (default when key absent).
    #[default]
    Normal,
    /// Skip lookup AND insert — always hit the provider, never populate cache.
    Bypass,
    /// Skip lookup, but DO insert (force-refresh stale entry).
    Refresh,
    /// Do lookup, but never insert (read-only cache consumer).
    #[serde(rename = "read-only")]
    ReadOnly,
}

/// Typed cache-control extracted from `tt_extras`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheControlConfig {
    /// Requested cache behaviour.
    #[serde(default)]
    pub mode: CacheMode,
    /// Override TTL for cache inserts. `None` = use the gateway default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

/// Parse [`CacheControlConfig`] from a request's `tt_extras` map.
///
/// Returns `None` when `tt_extras` does not contain a `"cache"` key.
/// Returns the default config (normal mode, no TTL override) when the key is
/// present but the value fails to deserialize — so a malformed field degrades
/// gracefully rather than hard-failing.
pub fn parse_cache_control(
    extras: &HashMap<String, serde_json::Value>,
) -> Option<CacheControlConfig> {
    let val = extras.get("cache")?;
    match serde_json::from_value::<CacheControlConfig>(val.clone()) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            // Log at warn level so operators can see bad payloads; fall back
            // to normal (don't block the request).
            tracing::warn!(
                error = %e,
                "tt_extras.cache deserialization failed — treating as normal"
            );
            Some(CacheControlConfig::default())
        }
    }
}

// ---------------------------------------------------------------------------
// tt_extras.panel types (Fusion panel)
// ---------------------------------------------------------------------------

/// Per-request panel overrides from `tt_extras.panel`.
///
/// JSON shape:
/// ```json
/// {
///   "panel": {
///     "members": ["gpt-4o", "claude-3-5-sonnet"],
///     "arbiter_model": "gpt-4o",
///     "quorum": 2,
///     "max_cost_usd": 0.05
///   }
/// }
/// ```
///
/// All fields are optional; absent fields fall back to gateway defaults in
/// `PanelConfig::resolve` (defined in `tt-core`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PanelExtras {
    /// Explicit list of member model ids to fan out to. Overrides the gateway
    /// default when non-empty.
    #[serde(default)]
    pub members: Vec<String>,

    /// Override the arbiter model for Synthesize / BestOfN strategies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arbiter_model: Option<String>,

    /// Minimum number of legs that must succeed for the panel to return a result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quorum: Option<usize>,

    /// Pre-dispatch admission budget in USD across all legs + arbitration.
    /// The gateway compares it with a static plan before dispatch; it is not a
    /// runtime spending cap, reservation, settlement, or provider-invoice
    /// guarantee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
}

/// Parse [`PanelExtras`] from a request's `tt_extras` map.
///
/// Returns `None` when `tt_extras` does not contain a `"panel"` key.
/// Returns `None` when the key is present but the value fails to deserialize —
/// the panel is an expensive opt-in path and must fail safe to "no panel" rather
/// than silently activating panel resolution with empty/default overrides.
pub fn parse_panel_extras(extras: &HashMap<String, serde_json::Value>) -> Option<PanelExtras> {
    let val = extras.get("panel")?;
    match serde_json::from_value::<PanelExtras>(val.clone()) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "tt_extras.panel deserialization failed — treating as no panel extras"
            );
            None
        }
    }
}

#[cfg(test)]
mod cache_control_tests {
    use super::*;

    fn extras(json: &str) -> HashMap<String, serde_json::Value> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn no_cache_key_returns_none() {
        assert!(parse_cache_control(&extras("{}")).is_none());
    }

    #[test]
    fn bypass_mode_parsed() {
        let cfg = parse_cache_control(&extras(r#"{"cache":{"mode":"bypass"}}"#)).unwrap();
        assert_eq!(cfg.mode, CacheMode::Bypass);
        assert!(cfg.ttl_secs.is_none());
    }

    #[test]
    fn refresh_mode_with_ttl() {
        let cfg = parse_cache_control(&extras(r#"{"cache":{"mode":"refresh","ttl_secs":3600}}"#))
            .unwrap();
        assert_eq!(cfg.mode, CacheMode::Refresh);
        assert_eq!(cfg.ttl_secs, Some(3600));
    }

    #[test]
    fn read_only_mode() {
        let cfg = parse_cache_control(&extras(r#"{"cache":{"mode":"read-only"}}"#)).unwrap();
        assert_eq!(cfg.mode, CacheMode::ReadOnly);
    }

    #[test]
    fn absent_mode_defaults_to_normal() {
        let cfg = parse_cache_control(&extras(r#"{"cache":{}}"#)).unwrap();
        assert_eq!(cfg.mode, CacheMode::Normal);
    }

    #[test]
    fn malformed_value_falls_back_to_default() {
        let cfg = parse_cache_control(&extras(r#"{"cache":"not-an-object"}"#)).unwrap();
        assert_eq!(cfg.mode, CacheMode::Normal);
    }
}

#[cfg(test)]
mod panel_extras_tests {
    use super::*;

    fn extras(json: &str) -> HashMap<String, serde_json::Value> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn absent_panel_key_is_none() {
        assert!(parse_panel_extras(&extras("{}")).is_none());
    }

    #[test]
    fn valid_panel_parses() {
        let panel = parse_panel_extras(&extras(
            r#"{"panel":{"members":["gpt-4o","claude-3-5-sonnet"],"quorum":2}}"#,
        ))
        .unwrap();
        assert_eq!(panel.members, vec!["gpt-4o", "claude-3-5-sonnet"]);
        assert_eq!(panel.quorum, Some(2));
    }

    #[test]
    fn malformed_panel_integer_value_is_none() {
        // panel value is a plain integer — fails to deserialize as PanelExtras.
        // Must NOT activate panel resolution (fail-safe to None).
        assert!(parse_panel_extras(&extras(r#"{"panel":42}"#)).is_none());
    }

    #[test]
    fn malformed_panel_string_value_is_none() {
        // panel value is a bare string — fails to deserialize as PanelExtras.
        // Must NOT activate panel resolution (fail-safe to None).
        assert!(parse_panel_extras(&extras(r#"{"panel":"not-an-object"}"#)).is_none());
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Newer OpenAI spend cap for reasoning models (`o3`, `o4-mini`, …). When a
    /// client sets this it MUST be honored end-to-end — dropping it silently
    /// removes the caller's output ceiling and changes spend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
    /// OpenAI `stream_options` (e.g. `{ "include_usage": true }`). Kept as an
    /// opaque value so the full object passes through to OpenAI-shaped upstreams
    /// unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Whether the model may emit tool calls in parallel (OpenAI
    /// `parallel_tool_calls`). Forwarded to OpenAI-shaped upstreams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Reasoning-effort hint for reasoning models (`"low"`/`"medium"`/`"high"`).
    /// Materially changes output and cost, so it must reach the upstream when
    /// supported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,

    /// TokenTrimmer-internal extras (cache config, route hints, etc.) that are
    /// stripped before forwarding to the provider.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tt_extras: HashMap<String, serde_json::Value>,

    /// Genuinely-unknown / newer OpenAI fields not modeled above. Captured via
    /// `#[serde(flatten)]` so they passthrough to OpenAI-shaped upstreams
    /// instead of being silently dropped on deserialize. Never includes the
    /// named fields above (serde consumes those first).
    #[serde(flatten, default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: MessageContent,
    },
    User {
        content: MessageContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Assistant {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<MessageContent>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Tool {
        content: MessageContent,
        tool_call_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    ImageUrl {
        image_url: ImageUrl,
    },
    InputAudio {
        input_audio: InputAudio,
    },
    /// A document (PDF / office file) input part — the Document Lane substrate
    /// (D4a). Accepts BOTH provider conventions for the same logical part:
    /// OpenAI's `{"type":"file","file":{...}}` and Anthropic's
    /// `{"type":"document","source":{...}}`. The variant tag aliases `file`, and
    /// the payload field aliases `file`/`source`, so either wire form
    /// deserializes into this one variant. The canonical serialization is
    /// `{"type":"document","document":{"source":{...},"filename":...}}` (see
    /// [`DocumentPart`]). In D4a this part only carries + routes; the pre-routing
    /// distillation seam that turns it into text is D4c.
    #[serde(alias = "file")]
    Document {
        #[serde(alias = "file", alias = "source")]
        document: DocumentPart,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Optional TokenTrimmer transport hint for remote image references whose
    /// path intentionally has no file extension (for example a content-addressed
    /// private object). Native adapters may use it to build their provider wire
    /// shape. OpenAI-compatible adapters remove it before upstream dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

pub const SUPPORTED_IMAGE_MEDIA_TYPES: [&str; 4] =
    ["image/jpeg", "image/png", "image/gif", "image/webp"];

/// Maximum decoded bytes in one inline image data URL.
pub const MAX_INLINE_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Maximum image content parts across one canonical chat request.
pub const MAX_IMAGE_PARTS_PER_REQUEST: usize = 4;

/// Maximum declared width or height for an inline image.
pub const MAX_INLINE_IMAGE_DIMENSION: u32 = 16_384;

/// Maximum declared decoded pixel surface for an inline image.
pub const MAX_INLINE_IMAGE_PIXELS: u64 = 40_000_000;

/// Maximum declared frames in one animated inline image.
pub const MAX_INLINE_IMAGE_FRAMES: u32 = 100;

const MAX_INLINE_IMAGE_BASE64_CHARS: usize = MAX_INLINE_IMAGE_BYTES.div_ceil(3) * 4;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImageUrlValidationError {
    #[error("image URL must be a valid HTTPS URL or a base64 data URL")]
    InvalidUrl,
    #[error("remote image URL must use HTTPS")]
    InsecureUrl,
    #[error("image URL must not contain embedded credentials")]
    EmbeddedCredentials,
    #[error("image media type must be image/jpeg, image/png, image/gif, or image/webp")]
    UnsupportedMediaType,
    #[error("image media type hint does not match the data URL media type")]
    MediaTypeMismatch,
    #[error("image data URL must contain valid standard base64")]
    InvalidBase64,
    #[error("inline image bytes exceed the 5 MiB per-image limit")]
    DecodedBytesExceeded,
    #[error("image bytes do not match the declared media type")]
    ContainerMismatch,
    #[error("image header must contain valid non-zero dimensions")]
    InvalidDimensions,
    #[error("inline image dimensions exceed 16384 pixels on an edge or 40000000 total pixels")]
    DimensionsExceeded,
    #[error("animated image container metadata is malformed or inconsistent")]
    InvalidAnimationMetadata,
    #[error("animated image exceeds the limit of 100 frames")]
    AnimationFramesExceeded,
    #[error("image request exceeds the limit of 4 image parts")]
    TooManyParts,
}

impl ImageUrl {
    /// Validate the provider-fetchable URL and optional TokenTrimmer MIME hint.
    ///
    /// Inline data-URL bytes are decoded, independently bounded, and checked
    /// against the declared shallow container signature. Remote URLs are not
    /// fetched, inspected, authorized, or attested here.
    pub fn validate(&self) -> Result<(), ImageUrlValidationError> {
        if self.url.starts_with("data:") {
            let (embedded_media_type, data) =
                parse_data_url(&self.url).ok_or(ImageUrlValidationError::InvalidUrl)?;
            validate_image_media_type(&embedded_media_type)?;
            if self
                .media_type
                .as_deref()
                .is_some_and(|hint| hint != embedded_media_type)
            {
                return Err(ImageUrlValidationError::MediaTypeMismatch);
            }
            if data.len() > MAX_INLINE_IMAGE_BASE64_CHARS {
                return Err(ImageUrlValidationError::DecodedBytesExceeded);
            }
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|_| ImageUrlValidationError::InvalidBase64)?;
            validate_image_bytes(&embedded_media_type, &decoded)?;
            return Ok(());
        }

        let parsed = url::Url::parse(&self.url).map_err(|_| ImageUrlValidationError::InvalidUrl)?;
        if parsed.scheme() != "https" || parsed.host_str().is_none() {
            return Err(ImageUrlValidationError::InsecureUrl);
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(ImageUrlValidationError::EmbeddedCredentials);
        }
        if let Some(media_type) = self.media_type.as_deref() {
            validate_image_media_type(media_type)?;
        }
        Ok(())
    }

    /// Exact MIME type for a native provider body when one is available.
    ///
    /// The explicit hint wins for a remote URL. A base64 data URL carries its
    /// own MIME type. Call [`Self::validate`] before using the result.
    #[must_use]
    pub fn effective_media_type(&self) -> Option<String> {
        self.media_type
            .clone()
            .or_else(|| parse_data_url(&self.url).map(|(media_type, _)| media_type))
    }
}

/// Validate already-decoded inline image bytes against the same bounded,
/// shallow container contract used for image data URLs.
pub fn validate_image_bytes(media_type: &str, bytes: &[u8]) -> Result<(), ImageUrlValidationError> {
    validate_image_media_type(media_type)?;
    if bytes.len() > MAX_INLINE_IMAGE_BYTES {
        return Err(ImageUrlValidationError::DecodedBytesExceeded);
    }
    if bytes.is_empty() || !image_signature_matches(media_type, bytes) {
        return Err(ImageUrlValidationError::ContainerMismatch);
    }
    let (width, height) =
        image_dimensions(media_type, bytes).ok_or(ImageUrlValidationError::InvalidDimensions)?;
    if width == 0 || height == 0 {
        return Err(ImageUrlValidationError::InvalidDimensions);
    }
    if width > MAX_INLINE_IMAGE_DIMENSION
        || height > MAX_INLINE_IMAGE_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_INLINE_IMAGE_PIXELS
    {
        return Err(ImageUrlValidationError::DimensionsExceeded);
    }
    validate_image_animation(media_type, bytes).map_err(|error| match error {
        ImageAnimationError::Invalid => ImageUrlValidationError::InvalidAnimationMetadata,
        ImageAnimationError::TooManyFrames => ImageUrlValidationError::AnimationFramesExceeded,
    })?;
    Ok(())
}

fn image_signature_matches(media_type: &str, bytes: &[u8]) -> bool {
    match media_type {
        "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP",
        _ => false,
    }
}

fn image_dimensions(media_type: &str, bytes: &[u8]) -> Option<(u32, u32)> {
    match media_type {
        "image/png" => png_dimensions(bytes),
        "image/jpeg" => jpeg_dimensions(bytes),
        "image/gif" => gif_dimensions(bytes),
        "image/webp" => webp_dimensions(bytes),
        _ => None,
    }
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24
        || !bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.get(12..16) != Some(b"IHDR")
    {
        return None;
    }
    Some((
        u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?),
        u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?),
    ))
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(b"\xff\xd8") {
        return None;
    }
    let mut index = 2usize;
    while index + 1 < bytes.len() {
        if bytes[index] != 0xff {
            index += 1;
            continue;
        }
        let mut marker_index = index;
        while marker_index < bytes.len() && bytes[marker_index] == 0xff {
            marker_index += 1;
        }
        let marker = *bytes.get(marker_index)?;
        if marker == 0xd8 || marker == 0xd9 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            index = marker_index + 1;
            continue;
        }
        let segment_len =
            u16::from_be_bytes([*bytes.get(marker_index + 1)?, *bytes.get(marker_index + 2)?])
                as usize;
        let is_start_of_frame =
            matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf);
        if is_start_of_frame {
            return Some((
                u32::from(u16::from_be_bytes([
                    *bytes.get(marker_index + 6)?,
                    *bytes.get(marker_index + 7)?,
                ])),
                u32::from(u16::from_be_bytes([
                    *bytes.get(marker_index + 4)?,
                    *bytes.get(marker_index + 5)?,
                ])),
            ));
        }
        index = marker_index
            .checked_add(1)?
            .checked_add(segment_len.max(2))?;
    }
    None
}

fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 10 || (!bytes.starts_with(b"GIF87a") && !bytes.starts_with(b"GIF89a")) {
        return None;
    }
    Some((
        u32::from(u16::from_le_bytes(bytes.get(6..8)?.try_into().ok()?)),
        u32::from(u16::from_le_bytes(bytes.get(8..10)?.try_into().ok()?)),
    ))
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 20 || !bytes.starts_with(b"RIFF") || bytes.get(8..12) != Some(b"WEBP") {
        return None;
    }
    match bytes.get(12..16)? {
        b"VP8X" => {
            let width = 1 + read_u24_le(bytes.get(24..27)?)?;
            let height = 1 + read_u24_le(bytes.get(27..30)?)?;
            Some((width, height))
        }
        b"VP8L" if bytes.get(20) == Some(&0x2f) => {
            let bits = bytes.get(21..25)?;
            let width = 1 + u32::from(bits[0]) + (u32::from(bits[1] & 0x3f) << 8);
            let height = 1
                + (u32::from(bits[1] >> 6)
                    | (u32::from(bits[2]) << 2)
                    | (u32::from(bits[3] & 0x0f) << 10));
            Some((width, height))
        }
        b"VP8 " if bytes.get(23..26) == Some(b"\x9d\x01\x2a") => Some((
            u32::from(u16::from_le_bytes(bytes.get(26..28)?.try_into().ok()?) & 0x3fff),
            u32::from(u16::from_le_bytes(bytes.get(28..30)?.try_into().ok()?) & 0x3fff),
        )),
        _ => None,
    }
}

fn read_u24_le(bytes: &[u8]) -> Option<u32> {
    Some(
        u32::from(*bytes.first()?)
            | (u32::from(*bytes.get(1)?) << 8)
            | (u32::from(*bytes.get(2)?) << 16),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageAnimationError {
    Invalid,
    TooManyFrames,
}

fn validate_image_animation(media_type: &str, bytes: &[u8]) -> Result<(), ImageAnimationError> {
    let frame_count = match media_type {
        "image/png" => png_frame_count(bytes),
        "image/gif" => gif_frame_count(bytes),
        "image/webp" => webp_frame_count(bytes),
        _ => return Ok(()),
    }
    .ok_or(ImageAnimationError::Invalid)?;
    if frame_count > MAX_INLINE_IMAGE_FRAMES {
        return Err(ImageAnimationError::TooManyFrames);
    }
    Ok(())
}

fn png_frame_count(bytes: &[u8]) -> Option<u32> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let mut offset = 8usize;
    let mut declared_frames = None;
    let mut frame_control_chunks = 0u32;
    let mut saw_end = false;
    while offset < bytes.len() {
        let header_end = offset.checked_add(8)?;
        let chunk_header = bytes.get(offset..header_end)?;
        let chunk_len =
            usize::try_from(u32::from_be_bytes(chunk_header[..4].try_into().ok()?)).ok()?;
        let data_end = header_end.checked_add(chunk_len)?;
        let chunk_end = data_end.checked_add(4)?;
        let chunk_data = bytes.get(header_end..data_end)?;
        let chunk_type = &chunk_header[4..8];
        match chunk_type {
            b"acTL" => {
                if declared_frames.is_some() || chunk_data.len() != 8 {
                    return None;
                }
                let count = u32::from_be_bytes(chunk_data[..4].try_into().ok()?);
                if count == 0 {
                    return None;
                }
                declared_frames = Some(count);
            }
            b"fcTL" => {
                if chunk_data.len() != 26 {
                    return None;
                }
                frame_control_chunks = frame_control_chunks.checked_add(1)?;
                if frame_control_chunks > MAX_INLINE_IMAGE_FRAMES {
                    return Some(frame_control_chunks);
                }
            }
            b"IEND" => {
                if !chunk_data.is_empty() || chunk_end != bytes.len() {
                    return None;
                }
                saw_end = true;
            }
            _ => {}
        }
        offset = chunk_end;
        if saw_end {
            break;
        }
    }
    if !saw_end {
        return None;
    }
    match declared_frames {
        Some(count) if count == frame_control_chunks => Some(count),
        Some(_) => None,
        None if frame_control_chunks == 0 => Some(1),
        None => None,
    }
}

fn gif_frame_count(bytes: &[u8]) -> Option<u32> {
    if bytes.len() < 13 || (!bytes.starts_with(b"GIF87a") && !bytes.starts_with(b"GIF89a")) {
        return None;
    }
    let packed = *bytes.get(10)?;
    let mut offset = 13usize;
    if packed & 0x80 != 0 {
        let table_entries = 1usize.checked_shl(u32::from((packed & 0x07) + 1))?;
        offset = offset.checked_add(table_entries.checked_mul(3)?)?;
    }
    if offset > bytes.len() {
        return None;
    }

    let mut frames = 0u32;
    loop {
        match *bytes.get(offset)? {
            0x2c => {
                let descriptor_end = offset.checked_add(10)?;
                let descriptor = bytes.get(offset..descriptor_end)?;
                offset = descriptor_end;
                if descriptor[9] & 0x80 != 0 {
                    let table_entries =
                        1usize.checked_shl(u32::from((descriptor[9] & 0x07) + 1))?;
                    offset = offset.checked_add(table_entries.checked_mul(3)?)?;
                }
                bytes.get(offset)?;
                offset = skip_gif_sub_blocks(bytes, offset.checked_add(1)?)?;
                frames = frames.checked_add(1)?;
                if frames > MAX_INLINE_IMAGE_FRAMES {
                    return Some(frames);
                }
            }
            0x21 => {
                bytes.get(offset.checked_add(1)?)?;
                offset = skip_gif_sub_blocks(bytes, offset.checked_add(2)?)?;
            }
            0x3b => {
                return (offset.checked_add(1)? == bytes.len() && frames > 0).then_some(frames);
            }
            _ => return None,
        }
    }
}

fn skip_gif_sub_blocks(bytes: &[u8], mut offset: usize) -> Option<usize> {
    loop {
        let len = usize::from(*bytes.get(offset)?);
        offset = offset.checked_add(1)?;
        if len == 0 {
            return Some(offset);
        }
        offset = offset.checked_add(len)?;
        bytes.get(..offset)?;
    }
}

fn webp_frame_count(bytes: &[u8]) -> Option<u32> {
    if bytes.len() < 20 || !bytes.starts_with(b"RIFF") || bytes.get(8..12) != Some(b"WEBP") {
        return None;
    }
    let riff_size = usize::try_from(u32::from_le_bytes(bytes.get(4..8)?.try_into().ok()?)).ok()?;
    if riff_size.checked_add(8)? != bytes.len() {
        return None;
    }
    let mut offset = 12usize;
    let mut animation_flag = false;
    let mut animation_header = false;
    let mut frames = 0u32;
    while offset < bytes.len() {
        let header_end = offset.checked_add(8)?;
        let header = bytes.get(offset..header_end)?;
        let chunk_len = usize::try_from(u32::from_le_bytes(header[4..8].try_into().ok()?)).ok()?;
        let data_end = header_end.checked_add(chunk_len)?;
        bytes.get(header_end..data_end)?;
        match &header[..4] {
            b"VP8X" => {
                if chunk_len != 10 {
                    return None;
                }
                animation_flag = bytes[header_end] & 0x02 != 0;
            }
            b"ANIM" => {
                if chunk_len != 6 {
                    return None;
                }
                animation_header = true;
            }
            b"ANMF" => {
                if chunk_len < 16 {
                    return None;
                }
                frames = frames.checked_add(1)?;
                if frames > MAX_INLINE_IMAGE_FRAMES {
                    return Some(frames);
                }
            }
            _ => {}
        }
        offset = data_end.checked_add(chunk_len % 2)?;
        if offset > bytes.len() {
            return None;
        }
    }
    if animation_flag {
        (animation_header && frames > 0).then_some(frames)
    } else if animation_header || frames > 0 {
        None
    } else {
        Some(1)
    }
}

fn validate_image_media_type(media_type: &str) -> Result<(), ImageUrlValidationError> {
    if SUPPORTED_IMAGE_MEDIA_TYPES.contains(&media_type) {
        Ok(())
    } else {
        Err(ImageUrlValidationError::UnsupportedMediaType)
    }
}

/// Validate every canonical image URL before routing, caching, sandboxing, or
/// Fusion fan-out. Provider adapters repeat this pure check when used directly.
pub fn validate_chat_image_urls(
    req: &ChatCompletionRequest,
) -> Result<(), ImageUrlValidationError> {
    let mut image_count = 0usize;
    for message in &req.messages {
        let content = match message {
            Message::System { content }
            | Message::User { content, .. }
            | Message::Tool { content, .. } => Some(content),
            Message::Assistant { content, .. } => content.as_ref(),
        };
        let Some(MessageContent::Parts(parts)) = content else {
            continue;
        };
        for part in parts {
            if let ContentPart::ImageUrl { image_url } = part {
                image_count += 1;
                if image_count > MAX_IMAGE_PARTS_PER_REQUEST {
                    return Err(ImageUrlValidationError::TooManyParts);
                }
                image_url.validate()?;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputAudio {
    /// Standard-padded base64 audio bytes. Data URLs and remote URLs are not
    /// part of the canonical OpenAI-compatible `input_audio` shape.
    pub data: String,
    /// Closed OpenAI-compatible audio format token (`"wav"` or `"mp3"`).
    pub format: String,
}

pub const SUPPORTED_INPUT_AUDIO_FORMATS: [&str; 2] = ["wav", "mp3"];

/// Maximum decoded canonical audio bytes across one chat request.
pub const MAX_INLINE_AUDIO_BYTES: usize = 20 * 1024 * 1024;

/// Maximum canonical audio content parts across one chat request.
pub const MAX_INPUT_AUDIO_PARTS_PER_REQUEST: usize = 4;

/// Maximum decoded playback duration across canonical audio parts in one chat request.
pub const MAX_INPUT_AUDIO_DURATION_SECONDS: u64 = 10 * 60;

/// Maximum declared sample rate for canonical audio input.
pub const MAX_INPUT_AUDIO_SAMPLE_RATE_HZ: u32 = 192_000;

/// Maximum declared channel count for canonical audio input.
pub const MAX_INPUT_AUDIO_CHANNELS: u16 = 8;

const MAX_INLINE_AUDIO_BASE64_CHARS: usize = MAX_INLINE_AUDIO_BYTES.div_ceil(3) * 4;
const MICROS_PER_SECOND: u64 = 1_000_000;
const MAX_INPUT_AUDIO_DURATION_MICROS: u64 = MAX_INPUT_AUDIO_DURATION_SECONDS * MICROS_PER_SECOND;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InputAudioValidationError {
    #[error("input_audio data must not be empty")]
    EmptyData,
    #[error("input_audio data must be standard base64 without a data-URL prefix")]
    InvalidBase64,
    #[error("input_audio format must be wav or mp3")]
    UnsupportedFormat,
    #[error("inline input_audio bytes exceed the 20 MiB request limit")]
    DecodedBytesExceeded,
    #[error("input_audio bytes do not match the declared wav or mp3 format")]
    FormatMismatch,
    #[error("input_audio container metadata is malformed or unsupported")]
    InvalidContainer,
    #[error("input_audio sample rate exceeds the 192 kHz limit")]
    SampleRateExceeded,
    #[error("input_audio channel count exceeds the limit of 8")]
    ChannelCountExceeded,
    #[error("input_audio request exceeds the 10-minute decoded-duration limit")]
    DurationExceeded,
    #[error("input_audio request exceeds the limit of 4 audio parts")]
    TooManyParts,
}

#[derive(Debug, Clone, Copy)]
struct ValidatedAudio {
    decoded_bytes: usize,
    duration_micros: u64,
}

impl InputAudio {
    /// Validate the canonical OpenAI-compatible audio input payload.
    ///
    /// Decoded bytes, duration, sample rate, and channel count are bounded.
    /// WAV chunk metadata and every MP3 frame are structurally walked without
    /// decoding samples. This does not scan content or attest provider acceptance.
    pub fn validate(&self) -> Result<(), InputAudioValidationError> {
        self.validate_with_metadata().map(|_| ())
    }

    fn validate_with_metadata(&self) -> Result<ValidatedAudio, InputAudioValidationError> {
        if !SUPPORTED_INPUT_AUDIO_FORMATS.contains(&self.format.as_str()) {
            return Err(InputAudioValidationError::UnsupportedFormat);
        }
        if self.data.is_empty() {
            return Err(InputAudioValidationError::EmptyData);
        }
        if self.data.len() > MAX_INLINE_AUDIO_BASE64_CHARS {
            return Err(InputAudioValidationError::DecodedBytesExceeded);
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&self.data)
            .map_err(|_| InputAudioValidationError::InvalidBase64)?;
        validate_input_audio_metadata(&self.format, &decoded)
    }
}

/// Validate already-decoded canonical audio bytes against the same bounded,
/// metadata-aware WAV/MP3 container contract used for `input_audio` base64.
pub fn validate_input_audio_bytes(
    format: &str,
    bytes: &[u8],
) -> Result<(), InputAudioValidationError> {
    validate_input_audio_metadata(format, bytes).map(|_| ())
}

fn validate_input_audio_metadata(
    format: &str,
    bytes: &[u8],
) -> Result<ValidatedAudio, InputAudioValidationError> {
    if !SUPPORTED_INPUT_AUDIO_FORMATS.contains(&format) {
        return Err(InputAudioValidationError::UnsupportedFormat);
    }
    if bytes.is_empty() {
        return Err(InputAudioValidationError::EmptyData);
    }
    if bytes.len() > MAX_INLINE_AUDIO_BYTES {
        return Err(InputAudioValidationError::DecodedBytesExceeded);
    }
    match format {
        "wav" => validate_wav_metadata(bytes),
        "mp3" => validate_mp3_metadata(bytes),
        _ => Err(InputAudioValidationError::UnsupportedFormat),
    }
}

fn validate_wav_metadata(bytes: &[u8]) -> Result<ValidatedAudio, InputAudioValidationError> {
    if bytes.len() < 12 || !bytes.starts_with(b"RIFF") || &bytes[8..12] != b"WAVE" {
        return Err(InputAudioValidationError::FormatMismatch);
    }

    let riff_size = usize::try_from(u32::from_le_bytes(
        bytes[4..8]
            .try_into()
            .map_err(|_| InputAudioValidationError::InvalidContainer)?,
    ))
    .map_err(|_| InputAudioValidationError::InvalidContainer)?;
    let riff_end = riff_size
        .checked_add(8)
        .ok_or(InputAudioValidationError::InvalidContainer)?;
    if riff_end != bytes.len() {
        return Err(InputAudioValidationError::InvalidContainer);
    }

    let mut offset = 12usize;
    let mut byte_rate = None;
    let mut data_bytes = 0u64;
    while offset < riff_end {
        let header_end = offset
            .checked_add(8)
            .ok_or(InputAudioValidationError::InvalidContainer)?;
        if header_end > riff_end {
            return Err(InputAudioValidationError::InvalidContainer);
        }
        let chunk_size = usize::try_from(u32::from_le_bytes(
            bytes[offset + 4..header_end]
                .try_into()
                .map_err(|_| InputAudioValidationError::InvalidContainer)?,
        ))
        .map_err(|_| InputAudioValidationError::InvalidContainer)?;
        let chunk_end = header_end
            .checked_add(chunk_size)
            .ok_or(InputAudioValidationError::InvalidContainer)?;
        if chunk_end > riff_end {
            return Err(InputAudioValidationError::InvalidContainer);
        }
        match &bytes[offset..offset + 4] {
            b"fmt " => {
                if byte_rate.is_some() || chunk_size < 16 {
                    return Err(InputAudioValidationError::InvalidContainer);
                }
                byte_rate = Some(validate_wav_format(&bytes[header_end..chunk_end])?);
            }
            b"data" => {
                data_bytes = data_bytes
                    .checked_add(
                        u64::try_from(chunk_size)
                            .map_err(|_| InputAudioValidationError::InvalidContainer)?,
                    )
                    .ok_or(InputAudioValidationError::InvalidContainer)?;
            }
            _ => {}
        }
        offset = chunk_end
            .checked_add(chunk_size % 2)
            .ok_or(InputAudioValidationError::InvalidContainer)?;
        if offset > riff_end {
            return Err(InputAudioValidationError::InvalidContainer);
        }
    }

    let byte_rate = u64::from(byte_rate.ok_or(InputAudioValidationError::InvalidContainer)?);
    if data_bytes == 0 {
        return Err(InputAudioValidationError::InvalidContainer);
    }
    let duration_micros = duration_micros(data_bytes, byte_rate)?;
    validate_audio_duration(duration_micros)?;
    Ok(ValidatedAudio {
        decoded_bytes: bytes.len(),
        duration_micros,
    })
}

fn validate_wav_format(format: &[u8]) -> Result<u32, InputAudioValidationError> {
    let read_u16 = |offset: usize| {
        format
            .get(offset..offset + 2)
            .and_then(|raw| raw.try_into().ok())
            .map(u16::from_le_bytes)
            .ok_or(InputAudioValidationError::InvalidContainer)
    };
    let read_u32 = |offset: usize| {
        format
            .get(offset..offset + 4)
            .and_then(|raw| raw.try_into().ok())
            .map(u32::from_le_bytes)
            .ok_or(InputAudioValidationError::InvalidContainer)
    };

    let mut encoding = read_u16(0)?;
    let channels = read_u16(2)?;
    let sample_rate = read_u32(4)?;
    let byte_rate = read_u32(8)?;
    let block_align = read_u16(12)?;
    let bits_per_sample = read_u16(14)?;
    if encoding == 0xfffe {
        if format.len() < 40 || read_u16(16)? < 22 {
            return Err(InputAudioValidationError::InvalidContainer);
        }
        encoding = read_u16(24)?;
    }
    if encoding != 1 && encoding != 3 {
        return Err(InputAudioValidationError::InvalidContainer);
    }
    if channels == 0 || sample_rate == 0 || bits_per_sample == 0 || bits_per_sample > 64 {
        return Err(InputAudioValidationError::InvalidContainer);
    }
    if channels > MAX_INPUT_AUDIO_CHANNELS {
        return Err(InputAudioValidationError::ChannelCountExceeded);
    }
    if sample_rate > MAX_INPUT_AUDIO_SAMPLE_RATE_HZ {
        return Err(InputAudioValidationError::SampleRateExceeded);
    }
    if encoding == 3 && bits_per_sample != 32 && bits_per_sample != 64 {
        return Err(InputAudioValidationError::InvalidContainer);
    }
    let bytes_per_sample = bits_per_sample.div_ceil(8);
    let expected_block_align = channels
        .checked_mul(bytes_per_sample)
        .ok_or(InputAudioValidationError::InvalidContainer)?;
    let expected_byte_rate = sample_rate
        .checked_mul(u32::from(expected_block_align))
        .ok_or(InputAudioValidationError::InvalidContainer)?;
    if block_align != expected_block_align || byte_rate != expected_byte_rate {
        return Err(InputAudioValidationError::InvalidContainer);
    }
    Ok(byte_rate)
}

fn validate_mp3_metadata(bytes: &[u8]) -> Result<ValidatedAudio, InputAudioValidationError> {
    let mut offset = 0usize;
    if bytes.starts_with(b"ID3") {
        if bytes.len() < 10
            || !(2..=4).contains(&bytes[3])
            || bytes[6..10].iter().any(|value| value & 0x80 != 0)
        {
            return Err(InputAudioValidationError::InvalidContainer);
        }
        let tag_size = bytes[6..10]
            .iter()
            .fold(0usize, |size, value| (size << 7) | usize::from(*value));
        let footer_size = usize::from(bytes[3] == 4 && bytes[5] & 0x10 != 0) * 10;
        offset = 10usize
            .checked_add(tag_size)
            .and_then(|value| value.checked_add(footer_size))
            .ok_or(InputAudioValidationError::InvalidContainer)?;
        if offset > bytes.len() {
            return Err(InputAudioValidationError::InvalidContainer);
        }
    } else if bytes.len() < 2 || bytes[0] != 0xff || bytes[1] & 0xe0 != 0xe0 {
        return Err(InputAudioValidationError::FormatMismatch);
    }

    let audio_end = if bytes.len().saturating_sub(offset) >= 128
        && bytes.get(bytes.len() - 128..bytes.len() - 125) == Some(b"TAG")
    {
        bytes.len() - 128
    } else {
        bytes.len()
    };
    let mut sample_rate = None;
    let mut channels = None;
    let mut total_samples = 0u64;
    while offset < audio_end {
        let header_end = offset
            .checked_add(4)
            .ok_or(InputAudioValidationError::InvalidContainer)?;
        let header_bytes: [u8; 4] = bytes
            .get(offset..header_end)
            .and_then(|raw| raw.try_into().ok())
            .ok_or(InputAudioValidationError::InvalidContainer)?;
        let header = u32::from_be_bytes(header_bytes);
        let frame = parse_mp3_frame_header(header)?;
        if let Some(rate) = sample_rate {
            if rate != frame.sample_rate_hz || channels != Some(frame.channels) {
                return Err(InputAudioValidationError::InvalidContainer);
            }
        } else {
            sample_rate = Some(frame.sample_rate_hz);
            channels = Some(frame.channels);
        }
        if frame.sample_rate_hz > MAX_INPUT_AUDIO_SAMPLE_RATE_HZ {
            return Err(InputAudioValidationError::SampleRateExceeded);
        }
        if frame.channels > MAX_INPUT_AUDIO_CHANNELS {
            return Err(InputAudioValidationError::ChannelCountExceeded);
        }
        offset = offset
            .checked_add(frame.frame_bytes)
            .ok_or(InputAudioValidationError::InvalidContainer)?;
        if offset > audio_end {
            return Err(InputAudioValidationError::InvalidContainer);
        }
        total_samples = total_samples
            .checked_add(u64::from(frame.samples_per_frame))
            .ok_or(InputAudioValidationError::InvalidContainer)?;
    }

    let sample_rate = u64::from(sample_rate.ok_or(InputAudioValidationError::InvalidContainer)?);
    let duration_micros = duration_micros(total_samples, sample_rate)?;
    validate_audio_duration(duration_micros)?;
    Ok(ValidatedAudio {
        decoded_bytes: bytes.len(),
        duration_micros,
    })
}

#[derive(Debug, Clone, Copy)]
struct Mp3Frame {
    frame_bytes: usize,
    samples_per_frame: u16,
    sample_rate_hz: u32,
    channels: u16,
}

fn parse_mp3_frame_header(header: u32) -> Result<Mp3Frame, InputAudioValidationError> {
    if header >> 21 != 0x7ff {
        return Err(InputAudioValidationError::InvalidContainer);
    }
    let version = (header >> 19) & 0x3;
    let layer = (header >> 17) & 0x3;
    let bitrate_index = usize::try_from((header >> 12) & 0xf)
        .map_err(|_| InputAudioValidationError::InvalidContainer)?;
    let sample_rate_index = usize::try_from((header >> 10) & 0x3)
        .map_err(|_| InputAudioValidationError::InvalidContainer)?;
    if version == 1 || layer != 1 || bitrate_index == 0 || bitrate_index == 15 {
        return Err(InputAudioValidationError::InvalidContainer);
    }
    let bitrates_kbps = if version == 3 {
        [
            0u32, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
        ]
    } else {
        [
            0u32, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
        ]
    };
    let base_sample_rates = [44_100u32, 48_000, 32_000];
    let base_sample_rate = *base_sample_rates
        .get(sample_rate_index)
        .ok_or(InputAudioValidationError::InvalidContainer)?;
    let sample_rate_hz = match version {
        3 => base_sample_rate,
        2 => base_sample_rate / 2,
        0 => base_sample_rate / 4,
        _ => return Err(InputAudioValidationError::InvalidContainer),
    };
    let bitrate_kbps = bitrates_kbps[bitrate_index];
    let padding = (header >> 9) & 1;
    let (coefficient, samples_per_frame) = if version == 3 {
        (144_000u32, 1_152u16)
    } else {
        (72_000u32, 576u16)
    };
    let frame_bytes = coefficient
        .checked_mul(bitrate_kbps)
        .and_then(|value| value.checked_div(sample_rate_hz))
        .and_then(|value| value.checked_add(padding))
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(InputAudioValidationError::InvalidContainer)?;
    if frame_bytes < 4 {
        return Err(InputAudioValidationError::InvalidContainer);
    }
    let channels = if (header >> 6) & 0x3 == 3 { 1 } else { 2 };
    Ok(Mp3Frame {
        frame_bytes,
        samples_per_frame,
        sample_rate_hz,
        channels,
    })
}

fn duration_micros(numerator: u64, denominator: u64) -> Result<u64, InputAudioValidationError> {
    if numerator == 0 || denominator == 0 {
        return Err(InputAudioValidationError::InvalidContainer);
    }
    numerator
        .checked_mul(MICROS_PER_SECOND)
        .and_then(|value| value.checked_add(denominator - 1))
        .and_then(|value| value.checked_div(denominator))
        .ok_or(InputAudioValidationError::InvalidContainer)
}

fn validate_audio_duration(duration_micros: u64) -> Result<(), InputAudioValidationError> {
    if duration_micros > MAX_INPUT_AUDIO_DURATION_MICROS {
        return Err(InputAudioValidationError::DurationExceeded);
    }
    Ok(())
}

/// Validate every canonical audio part before routing, caching, sandboxing, or
/// Fusion fan-out. Provider adapters repeat this pure check when used directly.
pub fn validate_chat_input_audio(
    req: &ChatCompletionRequest,
) -> Result<(), InputAudioValidationError> {
    let mut audio_count = 0usize;
    let mut decoded_bytes = 0usize;
    let mut duration_micros = 0u64;
    for message in &req.messages {
        let content = match message {
            Message::System { content }
            | Message::User { content, .. }
            | Message::Tool { content, .. } => Some(content),
            Message::Assistant { content, .. } => content.as_ref(),
        };
        let Some(MessageContent::Parts(parts)) = content else {
            continue;
        };
        for part in parts {
            if let ContentPart::InputAudio { input_audio } = part {
                audio_count += 1;
                if audio_count > MAX_INPUT_AUDIO_PARTS_PER_REQUEST {
                    return Err(InputAudioValidationError::TooManyParts);
                }
                let validated = input_audio.validate_with_metadata()?;
                decoded_bytes = decoded_bytes
                    .checked_add(validated.decoded_bytes)
                    .ok_or(InputAudioValidationError::DecodedBytesExceeded)?;
                if decoded_bytes > MAX_INLINE_AUDIO_BYTES {
                    return Err(InputAudioValidationError::DecodedBytesExceeded);
                }
                duration_micros = duration_micros
                    .checked_add(validated.duration_micros)
                    .ok_or(InputAudioValidationError::DurationExceeded)?;
                validate_audio_duration(duration_micros)?;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChatMediaValidationError {
    #[error(transparent)]
    Image(#[from] ImageUrlValidationError),
    #[error(transparent)]
    Audio(#[from] InputAudioValidationError),
    #[error(transparent)]
    Document(#[from] DocumentValidationError),
}

/// Validate every canonical image, audio, and document input through one seam.
pub fn validate_chat_media_inputs(
    req: &ChatCompletionRequest,
) -> Result<(), ChatMediaValidationError> {
    validate_chat_image_urls(req)?;
    validate_chat_input_audio(req)?;
    validate_chat_documents(req)?;
    Ok(())
}

/// A document input part's payload — the source bytes/URL plus an optional
/// filename. Mirrors [`ImageUrl`] for the document modality.
///
/// `DocumentPart` has a hand-written [`Deserialize`] that accepts the three wire
/// shapes the gateway may receive for the SAME logical document part:
/// 1. **Canonical / Anthropic-wrapped** — `{"source":{...},"filename"?}` (what
///    this type serializes to).
/// 2. **Anthropic bare source** — `{"type":"base64"|"url", ...}` (the payload
///    Anthropic nests under the block's `source` key, delivered here via the
///    `ContentPart::Document` field alias).
/// 3. **OpenAI file object** — `{"file_data":"data:<mime>;base64,<b64>",
///    "filename"?, "file_id"?}` (`file_data` data-URLs are split into a
///    [`DocumentSource::Base64`]).
///
/// Serialization is derived (the canonical shape #1), so a value parsed from any
/// provider form re-serializes to a stable canonical form that round-trips.
#[derive(Debug, Clone, Serialize)]
pub struct DocumentPart {
    pub source: DocumentSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

impl<'de> Deserialize<'de> for DocumentPart {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let value = serde_json::Value::deserialize(deserializer)?;
        let obj = value
            .as_object()
            .ok_or_else(|| D::Error::custom("document part must be a JSON object"))?;

        let filename = obj
            .get("filename")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        // Shape #1 (canonical / Anthropic-wrapped): an explicit `source` object.
        if let Some(src) = obj.get("source") {
            let source: DocumentSource =
                serde_json::from_value(src.clone()).map_err(D::Error::custom)?;
            return Ok(DocumentPart { source, filename });
        }

        // Shape #2 (Anthropic bare source): a `{"type":"base64"|"url", ...}`
        // object delivered directly (the block-level `source` value).
        if matches!(
            obj.get("type").and_then(serde_json::Value::as_str),
            Some("base64" | "url")
        ) {
            let source: DocumentSource =
                serde_json::from_value(value.clone()).map_err(D::Error::custom)?;
            return Ok(DocumentPart { source, filename });
        }

        // Shape #3 (OpenAI `file` object): a data-URL in `file_data`, else a
        // plain URL / `file_id` reference.
        if let Some(file_data) = obj.get("file_data").and_then(serde_json::Value::as_str) {
            let source = match parse_data_url(file_data) {
                Some((media_type, data)) => DocumentSource::Base64 { media_type, data },
                None => DocumentSource::Url {
                    url: file_data.to_string(),
                },
            };
            return Ok(DocumentPart { source, filename });
        }
        if let Some(file_id) = obj.get("file_id").and_then(serde_json::Value::as_str) {
            return Ok(DocumentPart {
                source: DocumentSource::Url {
                    url: file_id.to_string(),
                },
                filename,
            });
        }

        Err(D::Error::custom(
            "unrecognized document part shape (expected `source`, an Anthropic \
             `{type:base64|url}` source, or an OpenAI `file_data`/`file_id`)",
        ))
    }
}

/// Where a [`DocumentPart`]'s bytes come from: a remote/`data:` URL or inline
/// base64. Serializes to Anthropic's source convention
/// (`{"type":"url",...}` / `{"type":"base64","media_type":...,"data":...}`).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DocumentSource {
    /// Remote document URL (or an unparsed `data:` URL).
    Url { url: String },
    /// Inline base64-encoded document bytes with their media type.
    Base64 { media_type: String, data: String },
}

pub const SUPPORTED_DOCUMENT_MEDIA_TYPES: [&str; 7] = [
    "application/pdf",
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/bmp",
    "image/tiff",
];

/// Maximum decoded inline document bytes across one canonical chat request.
///
/// The gateway also has a whole-HTTP-body limit, but this modality-specific
/// ceiling remains enforceable when shared types are used outside that server.
pub const MAX_INLINE_DOCUMENT_BYTES: usize = 20 * 1024 * 1024;

/// Maximum document content parts across one canonical chat request.
pub const MAX_DOCUMENT_PARTS_PER_REQUEST: usize = 8;

/// Maximum PDF pages the isolated Document Lane parser will process.
pub const MAX_DOCUMENT_PAGES: u32 = 100;

/// Maximum UTF-8 bytes in one isolated Document Lane extraction response.
pub const MAX_DOCUMENT_EXTRACTED_TEXT_BYTES: usize = 4 * 1024 * 1024;

/// Maximum Unicode scalar values in one isolated Document Lane extraction response.
pub const MAX_DOCUMENT_EXTRACTED_TEXT_CHARS: usize = 1_000_000;

const MAX_INLINE_DOCUMENT_BASE64_CHARS: usize = MAX_INLINE_DOCUMENT_BYTES.div_ceil(3) * 4;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DocumentValidationError {
    #[error(
        "document media type must be application/pdf, image/png, image/jpeg, image/gif, image/webp, image/bmp, or image/tiff"
    )]
    UnsupportedMediaType,
    #[error("document base64 data must not be empty")]
    EmptyData,
    #[error("document data must be valid standard base64")]
    InvalidBase64,
    #[error("inline document bytes exceed the 20 MiB request limit")]
    DecodedBytesExceeded,
    #[error("document bytes do not match the declared media type")]
    MediaTypeMismatch,
    #[error("document image header must contain valid non-zero dimensions")]
    InvalidImageDimensions,
    #[error("document image dimensions exceed 16384 pixels on an edge or 40000000 total pixels")]
    ImageDimensionsExceeded,
    #[error("document animated-image container metadata is malformed or inconsistent")]
    InvalidImageAnimationMetadata,
    #[error("document animated image exceeds the limit of 100 frames")]
    ImageAnimationFramesExceeded,
    #[error("document request exceeds the limit of 8 document parts")]
    TooManyDocuments,
    #[error("document URL must be a valid HTTPS URL, base64 data URL, or OpenAI file id")]
    InvalidUrl,
    #[error("remote document URL must use HTTPS")]
    InsecureUrl,
    #[error("document URL must not contain embedded credentials")]
    EmbeddedCredentials,
    #[error("OpenAI document file id must start with file- and contain only ASCII letters, digits, underscore, or hyphen")]
    InvalidFileId,
}

impl DocumentSource {
    /// Validate the closed Document Lane v1 source contract.
    ///
    /// Base64/data-URL sources are syntax-checked against the media types the
    /// current sidecar understands. Remote sources must be HTTPS; `file-*`
    /// opaque IDs are accepted for OpenAI-compatible passthrough. Inline bytes
    /// are independently bounded and their container signature must match the
    /// declared media type. This does not deeply parse, scan, or attest the
    /// underlying document.
    pub fn validate(&self) -> Result<(), DocumentValidationError> {
        self.validate_with_inline_len().map(|_| ())
    }

    fn validate_with_inline_len(&self) -> Result<Option<usize>, DocumentValidationError> {
        match self {
            Self::Base64 { media_type, data } => {
                validate_document_base64(media_type, data).map(Some)
            }
            Self::Url { url } if url.starts_with("data:") => {
                let (media_type, data) =
                    parse_data_url(url).ok_or(DocumentValidationError::InvalidUrl)?;
                validate_document_base64(&media_type, &data).map(Some)
            }
            Self::Url { url } if url.starts_with("file-") => {
                if url.len() > 512
                    || url.len() <= "file-".len()
                    || !url
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                {
                    return Err(DocumentValidationError::InvalidFileId);
                }
                Ok(None)
            }
            Self::Url { url } => {
                let parsed =
                    url::Url::parse(url).map_err(|_| DocumentValidationError::InvalidUrl)?;
                if parsed.scheme() != "https" || parsed.host_str().is_none() {
                    return Err(DocumentValidationError::InsecureUrl);
                }
                if !parsed.username().is_empty() || parsed.password().is_some() {
                    return Err(DocumentValidationError::EmbeddedCredentials);
                }
                Ok(None)
            }
        }
    }
}

impl DocumentPart {
    pub fn validate(&self) -> Result<(), DocumentValidationError> {
        self.source.validate()
    }
}

fn validate_document_base64(
    media_type: &str,
    data: &str,
) -> Result<usize, DocumentValidationError> {
    if !SUPPORTED_DOCUMENT_MEDIA_TYPES.contains(&media_type) {
        return Err(DocumentValidationError::UnsupportedMediaType);
    }
    if data.is_empty() {
        return Err(DocumentValidationError::EmptyData);
    }
    // Refuse an obviously over-limit payload before allocating its decoded
    // representation. Standard base64 has no whitespace in this contract.
    if data.len() > MAX_INLINE_DOCUMENT_BASE64_CHARS {
        return Err(DocumentValidationError::DecodedBytesExceeded);
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| DocumentValidationError::InvalidBase64)?;
    validate_document_bytes(media_type, &decoded)?;
    Ok(decoded.len())
}

/// Validate already-decoded inline document bytes against the same bounded
/// media/container contract used for canonical base64 document parts.
pub fn validate_document_bytes(
    media_type: &str,
    bytes: &[u8],
) -> Result<(), DocumentValidationError> {
    if !SUPPORTED_DOCUMENT_MEDIA_TYPES.contains(&media_type) {
        return Err(DocumentValidationError::UnsupportedMediaType);
    }
    if bytes.is_empty() {
        return Err(DocumentValidationError::EmptyData);
    }
    if bytes.len() > MAX_INLINE_DOCUMENT_BYTES {
        return Err(DocumentValidationError::DecodedBytesExceeded);
    }
    if !document_signature_matches(media_type, bytes) {
        return Err(DocumentValidationError::MediaTypeMismatch);
    }
    if media_type.starts_with("image/") {
        let (width, height) = document_image_dimensions(media_type, bytes)
            .ok_or(DocumentValidationError::InvalidImageDimensions)?;
        if width == 0 || height == 0 {
            return Err(DocumentValidationError::InvalidImageDimensions);
        }
        if width > MAX_INLINE_IMAGE_DIMENSION
            || height > MAX_INLINE_IMAGE_DIMENSION
            || u64::from(width) * u64::from(height) > MAX_INLINE_IMAGE_PIXELS
        {
            return Err(DocumentValidationError::ImageDimensionsExceeded);
        }
        validate_image_animation(media_type, bytes).map_err(|error| match error {
            ImageAnimationError::Invalid => DocumentValidationError::InvalidImageAnimationMetadata,
            ImageAnimationError::TooManyFrames => {
                DocumentValidationError::ImageAnimationFramesExceeded
            }
        })?;
    }
    Ok(())
}

fn document_signature_matches(media_type: &str, bytes: &[u8]) -> bool {
    match media_type {
        // ISO 32000 readers allow the header to occur within the first 1024
        // bytes, so do not require it at offset zero.
        "application/pdf" => bytes[..bytes.len().min(1024)]
            .windows(b"%PDF-".len())
            .any(|window| window == b"%PDF-"),
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP",
        "image/bmp" => bytes.starts_with(b"BM"),
        "image/tiff" => bytes.starts_with(b"II\x2a\x00") || bytes.starts_with(b"MM\x00\x2a"),
        _ => false,
    }
}

fn document_image_dimensions(media_type: &str, bytes: &[u8]) -> Option<(u32, u32)> {
    image_dimensions(media_type, bytes).or_else(|| match media_type {
        "image/bmp" => bmp_dimensions(bytes),
        "image/tiff" => tiff_dimensions(bytes),
        _ => None,
    })
}

fn bmp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 22 || !bytes.starts_with(b"BM") {
        return None;
    }
    let dib_size = u32::from_le_bytes(bytes.get(14..18)?.try_into().ok()?);
    if dib_size == 12 {
        return Some((
            u32::from(u16::from_le_bytes(bytes.get(18..20)?.try_into().ok()?)),
            u32::from(u16::from_le_bytes(bytes.get(20..22)?.try_into().ok()?)),
        ));
    }
    if dib_size < 40 || bytes.len() < 26 {
        return None;
    }
    let width = i32::from_le_bytes(bytes.get(18..22)?.try_into().ok()?);
    let height = i32::from_le_bytes(bytes.get(22..26)?.try_into().ok()?);
    if width <= 0 || height == 0 {
        return None;
    }
    Some((u32::try_from(width).ok()?, height.unsigned_abs()))
}

fn tiff_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let little_endian = match bytes.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    if read_endian_u16(bytes, 2, little_endian)? != 42 {
        return None;
    }
    let ifd_offset = usize::try_from(read_endian_u32(bytes, 4, little_endian)?).ok()?;
    let entry_count = usize::from(read_endian_u16(bytes, ifd_offset, little_endian)?);
    let mut width = None;
    let mut height = None;
    for index in 0..entry_count {
        let entry_offset = ifd_offset
            .checked_add(2)?
            .checked_add(index.checked_mul(12)?)?;
        let tag = read_endian_u16(bytes, entry_offset, little_endian)?;
        if tag != 256 && tag != 257 {
            continue;
        }
        let field_type = read_endian_u16(bytes, entry_offset + 2, little_endian)?;
        let count = read_endian_u32(bytes, entry_offset + 4, little_endian)?;
        if count != 1 {
            continue;
        }
        let value = match field_type {
            3 => u32::from(read_endian_u16(bytes, entry_offset + 8, little_endian)?),
            4 => read_endian_u32(bytes, entry_offset + 8, little_endian)?,
            _ => continue,
        };
        match tag {
            256 => width = Some(value),
            257 => height = Some(value),
            _ => {}
        }
        if width.is_some() && height.is_some() {
            break;
        }
    }
    Some((width?, height?))
}

fn read_endian_u16(bytes: &[u8], offset: usize, little_endian: bool) -> Option<u16> {
    let raw: [u8; 2] = bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?;
    Some(if little_endian {
        u16::from_le_bytes(raw)
    } else {
        u16::from_be_bytes(raw)
    })
}

fn read_endian_u32(bytes: &[u8], offset: usize, little_endian: bool) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
    Some(if little_endian {
        u32::from_le_bytes(raw)
    } else {
        u32::from_be_bytes(raw)
    })
}

/// Validate every canonical document before routing, caching, sandboxing, or
/// Fusion fan-out. Provider adapters repeat this pure check when used directly.
pub fn validate_chat_documents(req: &ChatCompletionRequest) -> Result<(), DocumentValidationError> {
    let mut document_count = 0usize;
    let mut inline_bytes = 0usize;
    for message in &req.messages {
        let content = match message {
            Message::System { content }
            | Message::User { content, .. }
            | Message::Tool { content, .. } => Some(content),
            Message::Assistant { content, .. } => content.as_ref(),
        };
        let Some(MessageContent::Parts(parts)) = content else {
            continue;
        };
        for part in parts {
            if let ContentPart::Document { document } = part {
                document_count += 1;
                if document_count > MAX_DOCUMENT_PARTS_PER_REQUEST {
                    return Err(DocumentValidationError::TooManyDocuments);
                }
                if let Some(decoded_len) = document.source.validate_with_inline_len()? {
                    inline_bytes = inline_bytes
                        .checked_add(decoded_len)
                        .ok_or(DocumentValidationError::DecodedBytesExceeded)?;
                    if inline_bytes > MAX_INLINE_DOCUMENT_BYTES {
                        return Err(DocumentValidationError::DecodedBytesExceeded);
                    }
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Tool {
    #[serde(rename = "type")]
    pub r#type: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolFunction {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolChoice {
    Auto(String),
    Specific {
        #[serde(rename = "type")]
        r#type: String,
        function: ToolChoiceFunction,
    },
}

impl ToolChoice {
    /// Let the model decide whether to call a tool (`"auto"`).
    #[must_use]
    pub fn auto() -> Self {
        ToolChoice::Auto("auto".to_string())
    }

    /// Forbid tool calls — force a plain text answer (`"none"`).
    #[must_use]
    pub fn none() -> Self {
        ToolChoice::Auto("none".to_string())
    }

    /// Require the model to call some tool (`"required"`).
    #[must_use]
    pub fn required() -> Self {
        ToolChoice::Auto("required".to_string())
    }

    /// Require the model to call a specific named function.
    #[must_use]
    pub fn function(name: impl Into<String>) -> Self {
        ToolChoice::Specific {
            r#type: "function".to_string(),
            function: ToolChoiceFunction { name: name.into() },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolChoiceFunction {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// Stringified JSON arguments — OpenAI convention.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseFormat {
    #[serde(rename = "type")]
    pub r#type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub index: u32,
    pub message: Message,
    pub finish_reason: Option<String>,
}

/// One SSE event from a streaming chat completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Genuinely-unknown / newer provider chunk fields (e.g.
    /// `system_fingerprint`, `service_tier`). Captured via `#[serde(flatten)]`
    /// so upstream SSE chunks round-trip through the gateway unchanged rather
    /// than being silently dropped.
    #[serde(flatten, default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: ChunkDelta,
    pub finish_reason: Option<String>,
    /// Unknown per-choice fields (e.g. `logprobs`) preserved verbatim.
    #[serde(flatten, default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChunkDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Unknown per-delta fields (e.g. `refusal`) preserved verbatim.
    #[serde(flatten, default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingsRequest {
    pub model: String,
    pub input: EmbeddingInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding_format: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    Single(String),
    Batch(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingsResponse {
    pub object: String,
    pub data: Vec<EmbeddingData>,
    pub model: String,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingData {
    pub object: String,
    pub index: u32,
    pub embedding: Vec<f32>,
}

/// Parse a base64 `data:` URL into `(media_type, base64_payload)`.
///
/// Returns `None` for non-`data:` URLs, non-base64 data URLs, or a malformed/
/// empty media type. Provider adapters use this to forward inline image bytes
/// as the provider's native base64 image part instead of mistakenly sending the
/// whole `data:` URI as a *remote* URL reference (which the upstream rejects).
#[must_use]
pub fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    // Only base64 payloads are supported (the canonical image transport).
    let media_with_params = meta.strip_suffix(";base64")?;
    // Drop any RFC-2397 media-type parameters (e.g. `;charset=utf-8`) — providers
    // expect a bare MIME type like `image/png` in the base64 image part.
    let media_type = media_with_params.split(';').next().unwrap_or("");
    if media_type.is_empty() || data.is_empty() {
        return None;
    }
    Some((media_type.to_string(), data.to_string()))
}

#[cfg(test)]
pub(super) mod embeddings_default_tests {
    use super::*;

    fn wav_bytes(
        data_len: usize,
        channels: u16,
        sample_rate: u32,
        bits_per_sample: u16,
    ) -> Vec<u8> {
        let bytes_per_sample = bits_per_sample.div_ceil(8);
        let block_align = channels.checked_mul(bytes_per_sample).unwrap();
        let byte_rate = sample_rate.checked_mul(u32::from(block_align)).unwrap();
        let padding = data_len % 2;
        let file_len = 44usize
            .checked_add(data_len)
            .unwrap()
            .checked_add(padding)
            .unwrap();
        let riff_size = u32::try_from(file_len - 8).unwrap();
        let data_size = u32::try_from(data_len).unwrap();
        let mut bytes = Vec::with_capacity(file_len);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&riff_size.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&byte_rate.to_le_bytes());
        bytes.extend_from_slice(&block_align.to_le_bytes());
        bytes.extend_from_slice(&bits_per_sample.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_size.to_le_bytes());
        bytes.resize(44 + data_len, 0);
        if padding != 0 {
            bytes.push(0);
        }
        bytes
    }

    fn mp3_bytes(frame_count: usize) -> Vec<u8> {
        const FRAME_BYTES: usize = 417;
        let mut bytes = Vec::with_capacity(frame_count * FRAME_BYTES);
        for _ in 0..frame_count {
            let offset = bytes.len();
            bytes.resize(offset + FRAME_BYTES, 0);
            bytes[offset..offset + 4].copy_from_slice(b"\xff\xfb\x90\x64");
        }
        bytes
    }

    fn append_png_chunk(bytes: &mut Vec<u8>, chunk_type: &[u8; 4], data: &[u8]) {
        bytes.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(chunk_type);
        bytes.extend_from_slice(data);
        bytes.extend_from_slice(&[0; 4]);
    }

    pub(super) fn png_bytes(frame_count: Option<u32>) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = [0u8; 13];
        ihdr[..4].copy_from_slice(&1u32.to_be_bytes());
        ihdr[4..8].copy_from_slice(&1u32.to_be_bytes());
        append_png_chunk(&mut bytes, b"IHDR", &ihdr);
        if let Some(frame_count) = frame_count {
            let mut animation_control = [0u8; 8];
            animation_control[..4].copy_from_slice(&frame_count.to_be_bytes());
            append_png_chunk(&mut bytes, b"acTL", &animation_control);
            for sequence in 0..frame_count {
                let mut frame_control = [0u8; 26];
                frame_control[..4].copy_from_slice(&sequence.to_be_bytes());
                frame_control[4..8].copy_from_slice(&1u32.to_be_bytes());
                frame_control[8..12].copy_from_slice(&1u32.to_be_bytes());
                append_png_chunk(&mut bytes, b"fcTL", &frame_control);
            }
        }
        append_png_chunk(&mut bytes, b"IEND", &[]);
        bytes
    }

    pub(super) fn gif_bytes(frame_count: u32) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&[0; 3]);
        for _ in 0..frame_count {
            bytes.push(0x2c);
            bytes.extend_from_slice(&[0; 4]);
            bytes.extend_from_slice(&1u16.to_le_bytes());
            bytes.extend_from_slice(&1u16.to_le_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&[2, 1, 0, 0]);
        }
        bytes.push(0x3b);
        bytes
    }

    fn append_webp_chunk(bytes: &mut Vec<u8>, chunk_type: &[u8; 4], data: &[u8]) {
        bytes.extend_from_slice(chunk_type);
        bytes.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(data);
        if data.len() % 2 != 0 {
            bytes.push(0);
        }
    }

    pub(super) fn webp_bytes(frame_count: Option<u32>) -> Vec<u8> {
        let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
        let mut extended = [0u8; 10];
        extended[0] = u8::from(frame_count.is_some()) * 0x02;
        append_webp_chunk(&mut bytes, b"VP8X", &extended);
        if let Some(frame_count) = frame_count {
            append_webp_chunk(&mut bytes, b"ANIM", &[0; 6]);
            for _ in 0..frame_count {
                append_webp_chunk(&mut bytes, b"ANMF", &[0; 16]);
            }
        }
        let riff_size = u32::try_from(bytes.len() - 8).unwrap();
        bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
        bytes
    }

    #[test]
    fn chat_request_default_is_empty() {
        let r = ChatCompletionRequest::default();
        assert_eq!(r.model, "");
        assert!(r.messages.is_empty());
        assert!(!r.stream);
        assert!(r.tools.is_empty());
        assert!(r.max_tokens.is_none());
    }

    #[test]
    fn typed_compat_fields_roundtrip() {
        let json = serde_json::json!({
            "model": "o3",
            "messages": [{ "role": "user", "content": "hi" }],
            "max_completion_tokens": 4096,
            "stream_options": { "include_usage": true },
            "parallel_tool_calls": false,
            "reasoning_effort": "high",
        });
        let req: ChatCompletionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.max_completion_tokens, Some(4096));
        assert_eq!(req.parallel_tool_calls, Some(false));
        assert_eq!(req.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            req.stream_options,
            Some(serde_json::json!({ "include_usage": true }))
        );
        // The flatten map must NOT capture the named fields.
        assert!(req.extra.is_empty());

        let out = serde_json::to_value(&req).unwrap();
        assert_eq!(out["max_completion_tokens"], 4096);
        assert_eq!(
            out["stream_options"],
            serde_json::json!({"include_usage": true})
        );
        assert_eq!(out["parallel_tool_calls"], false);
        assert_eq!(out["reasoning_effort"], "high");
    }

    #[test]
    fn unknown_fields_passthrough_via_flatten() {
        // A genuinely-unknown / newer OpenAI field must survive deserialize and
        // re-serialize verbatim rather than being silently dropped.
        let json = serde_json::json!({
            "model": "gpt-4o",
            "messages": [{ "role": "user", "content": "hi" }],
            "logprobs": true,
            "top_logprobs": 5,
            "service_tier": "auto",
        });
        let req: ChatCompletionRequest = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(req.extra.get("logprobs"), Some(&serde_json::json!(true)));
        assert_eq!(req.extra.get("top_logprobs"), Some(&serde_json::json!(5)));
        assert_eq!(
            req.extra.get("service_tier"),
            Some(&serde_json::json!("auto"))
        );

        let out = serde_json::to_value(&req).unwrap();
        assert_eq!(out["logprobs"], true);
        assert_eq!(out["top_logprobs"], 5);
        assert_eq!(out["service_tier"], "auto");
    }

    #[test]
    fn streaming_chunk_unknown_fields_passthrough() {
        // Unknown / newer provider fields on a streaming chunk (and on its nested
        // choice/delta) must survive deserialize and re-serialize verbatim rather
        // than being silently dropped on the round-trip passthrough.
        let json = serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion.chunk",
            "created": 1716598234,
            "model": "gpt-4o",
            "system_fingerprint": "fp_abc123",
            "choices": [{
                "index": 0,
                "delta": { "content": "hi", "refusal": null },
                "finish_reason": null,
                "logprobs": { "content": [] }
            }]
        });
        let chunk: ChatCompletionChunk = serde_json::from_value(json).unwrap();
        assert_eq!(
            chunk.extra.get("system_fingerprint"),
            Some(&serde_json::json!("fp_abc123"))
        );
        assert_eq!(
            chunk.choices[0].extra.get("logprobs"),
            Some(&serde_json::json!({ "content": [] }))
        );
        assert_eq!(
            chunk.choices[0].delta.extra.get("refusal"),
            Some(&serde_json::Value::Null)
        );

        let out = serde_json::to_value(&chunk).unwrap();
        assert_eq!(out["system_fingerprint"], "fp_abc123");
        assert_eq!(
            out["choices"][0]["logprobs"],
            serde_json::json!({ "content": [] })
        );
        assert_eq!(
            out["choices"][0]["delta"]["refusal"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn parse_data_url_extracts_media_type_and_payload() {
        assert_eq!(
            parse_data_url("data:image/png;base64,iVBORw0KGgo="),
            Some(("image/png".to_string(), "iVBORw0KGgo=".to_string()))
        );
        // Non-data URLs and non-base64 / malformed data URLs return None.
        assert_eq!(parse_data_url("https://example.com/cat.png"), None);
        assert_eq!(parse_data_url("data:image/png,notbase64"), None);
        assert_eq!(parse_data_url("data:;base64,abc"), None);
        assert_eq!(parse_data_url("data:image/png;base64,"), None);
        // Media-type parameters are stripped to a bare MIME type.
        assert_eq!(
            parse_data_url("data:image/png;charset=utf-8;base64,iVBORw0KGgo="),
            Some(("image/png".to_string(), "iVBORw0KGgo=".to_string()))
        );
    }

    #[test]
    fn image_media_type_hint_roundtrips_and_validates_extensionless_https() {
        let json = serde_json::json!({
            "model": "gemini-3.1-pro",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image_url",
                    "image_url": {
                        "url": "https://objects.example.test/private/digest?X-Amz-Signature=abc",
                        "media_type": "image/png"
                    }
                }]
            }]
        });
        let request: ChatCompletionRequest = serde_json::from_value(json).unwrap();
        validate_chat_image_urls(&request).expect("valid private image reference");
        let encoded = serde_json::to_value(request).unwrap();
        assert_eq!(
            encoded["messages"][0]["content"][0]["image_url"]["media_type"],
            "image/png"
        );
    }

    #[test]
    fn image_url_validation_rejects_unsafe_or_inconsistent_hints() {
        let request = |url: &str, media_type: Option<&str>| ChatCompletionRequest {
            messages: vec![Message::User {
                content: MessageContent::Parts(vec![ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: url.to_string(),
                        detail: None,
                        media_type: media_type.map(str::to_string),
                    },
                }]),
                name: None,
            }],
            ..Default::default()
        };

        assert_eq!(
            validate_chat_image_urls(&request("http://example.com/image.png", Some("image/png"))),
            Err(ImageUrlValidationError::InsecureUrl)
        );
        assert_eq!(
            validate_chat_image_urls(&request(
                "data:image/png;base64,iVBORw0KGgo=",
                Some("image/jpeg"),
            )),
            Err(ImageUrlValidationError::MediaTypeMismatch)
        );
        assert_eq!(
            validate_chat_image_urls(&request(
                "https://example.com/image.svg",
                Some("image/svg+xml"),
            )),
            Err(ImageUrlValidationError::UnsupportedMediaType)
        );
        assert_eq!(
            validate_chat_image_urls(&request(
                "https://user:password@example.com/image.png",
                Some("image/png"),
            )),
            Err(ImageUrlValidationError::EmbeddedCredentials)
        );
    }

    #[test]
    fn image_data_urls_enforce_base64_container_size_and_count_limits() {
        let mut png = png_bytes(None);

        let jpeg = vec![
            0xff, 0xd8, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x01, 0x00, 0x01,
        ];

        let gif = gif_bytes(1);

        let webp = webp_bytes(None);

        let mut webp_lossless = vec![0; 26];
        webp_lossless[..4].copy_from_slice(b"RIFF");
        webp_lossless[4..8].copy_from_slice(&18u32.to_le_bytes());
        webp_lossless[8..12].copy_from_slice(b"WEBP");
        webp_lossless[12..16].copy_from_slice(b"VP8L");
        webp_lossless[16..20].copy_from_slice(&5u32.to_le_bytes());
        webp_lossless[20] = 0x2f;

        let mut webp_lossy = vec![0; 30];
        webp_lossy[..4].copy_from_slice(b"RIFF");
        webp_lossy[4..8].copy_from_slice(&22u32.to_le_bytes());
        webp_lossy[8..12].copy_from_slice(b"WEBP");
        webp_lossy[12..16].copy_from_slice(b"VP8 ");
        webp_lossy[16..20].copy_from_slice(&10u32.to_le_bytes());
        webp_lossy[23..26].copy_from_slice(b"\x9d\x01\x2a");
        webp_lossy[26..28].copy_from_slice(&1u16.to_le_bytes());
        webp_lossy[28..30].copy_from_slice(&1u16.to_le_bytes());

        for (media_type, bytes) in [
            ("image/jpeg", jpeg.as_slice()),
            ("image/png", png.as_slice()),
            ("image/gif", gif.as_slice()),
            ("image/webp", webp.as_slice()),
            ("image/webp", webp_lossless.as_slice()),
            ("image/webp", webp_lossy.as_slice()),
        ] {
            validate_image_bytes(media_type, bytes)
                .unwrap_or_else(|error| panic!("{media_type} signature rejected: {error}"));
        }
        assert_eq!(
            validate_image_bytes("image/png", b"\xff\xd8\xff"),
            Err(ImageUrlValidationError::ContainerMismatch)
        );
        assert_eq!(
            validate_image_bytes("image/png", b"\x89PNG\r\n\x1a\n"),
            Err(ImageUrlValidationError::InvalidDimensions)
        );
        png[16..20].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(
            validate_image_bytes("image/png", &png),
            Err(ImageUrlValidationError::InvalidDimensions)
        );
        png[16..20].copy_from_slice(&(MAX_INLINE_IMAGE_DIMENSION + 1).to_be_bytes());
        assert_eq!(
            validate_image_bytes("image/png", &png),
            Err(ImageUrlValidationError::DimensionsExceeded)
        );
        png[16..20].copy_from_slice(&10_000u32.to_be_bytes());
        png[20..24].copy_from_slice(&10_000u32.to_be_bytes());
        assert_eq!(
            validate_image_bytes("image/png", &png),
            Err(ImageUrlValidationError::DimensionsExceeded)
        );
        assert_eq!(
            validate_image_bytes("image/png", &vec![0; MAX_INLINE_IMAGE_BYTES + 1]),
            Err(ImageUrlValidationError::DecodedBytesExceeded)
        );

        let request = |urls: Vec<String>| ChatCompletionRequest {
            messages: vec![Message::User {
                content: MessageContent::Parts(
                    urls.into_iter()
                        .map(|url| ContentPart::ImageUrl {
                            image_url: ImageUrl {
                                url,
                                detail: None,
                                media_type: None,
                            },
                        })
                        .collect(),
                ),
                name: None,
            }],
            ..Default::default()
        };
        assert_eq!(
            validate_chat_image_urls(&request(vec!["data:image/png;base64,not-base64!".into()])),
            Err(ImageUrlValidationError::InvalidBase64)
        );
        assert_eq!(
            validate_chat_image_urls(&request(vec!["data:image/png;base64,/9j/4A==".into()])),
            Err(ImageUrlValidationError::ContainerMismatch)
        );
        assert_eq!(
            validate_chat_image_urls(&request(
                (0..=MAX_IMAGE_PARTS_PER_REQUEST)
                    .map(|index| format!("https://example.com/image-{index}.png"))
                    .collect()
            )),
            Err(ImageUrlValidationError::TooManyParts)
        );
    }

    #[test]
    fn animated_images_enforce_bounded_consistent_frame_metadata() {
        for (media_type, bytes) in [
            ("image/png", png_bytes(Some(MAX_INLINE_IMAGE_FRAMES))),
            ("image/gif", gif_bytes(MAX_INLINE_IMAGE_FRAMES)),
            ("image/webp", webp_bytes(Some(MAX_INLINE_IMAGE_FRAMES))),
        ] {
            validate_image_bytes(media_type, &bytes)
                .unwrap_or_else(|error| panic!("{media_type} frame limit rejected: {error}"));
            validate_document_bytes(media_type, &bytes).unwrap_or_else(|error| {
                panic!("document-wrapped {media_type} frame limit rejected: {error}")
            });
        }

        for (media_type, bytes) in [
            ("image/png", png_bytes(Some(MAX_INLINE_IMAGE_FRAMES + 1))),
            ("image/gif", gif_bytes(MAX_INLINE_IMAGE_FRAMES + 1)),
            ("image/webp", webp_bytes(Some(MAX_INLINE_IMAGE_FRAMES + 1))),
        ] {
            assert_eq!(
                validate_image_bytes(media_type, &bytes),
                Err(ImageUrlValidationError::AnimationFramesExceeded)
            );
            assert_eq!(
                validate_document_bytes(media_type, &bytes),
                Err(DocumentValidationError::ImageAnimationFramesExceeded)
            );
        }

        let mut inconsistent_png = png_bytes(Some(2));
        let first_frame_control = inconsistent_png
            .windows(4)
            .position(|window| window == b"fcTL")
            .unwrap();
        inconsistent_png.drain(first_frame_control - 4..first_frame_control + 34);
        assert_eq!(
            validate_image_bytes("image/png", &inconsistent_png),
            Err(ImageUrlValidationError::InvalidAnimationMetadata)
        );

        let mut inconsistent_webp = webp_bytes(Some(1));
        inconsistent_webp[20] &= !0x02;
        assert_eq!(
            validate_image_bytes("image/webp", &inconsistent_webp),
            Err(ImageUrlValidationError::InvalidAnimationMetadata)
        );
    }

    #[test]
    fn input_audio_roundtrips_and_accepts_only_valid_base64_wav_or_mp3() {
        let request = |data: &str, format: &str| ChatCompletionRequest {
            messages: vec![Message::User {
                content: MessageContent::Parts(vec![ContentPart::InputAudio {
                    input_audio: InputAudio {
                        data: data.to_string(),
                        format: format.to_string(),
                    },
                }]),
                name: None,
            }],
            ..Default::default()
        };

        for (format, bytes) in [("wav", wav_bytes(2, 1, 16_000, 16)), ("mp3", mp3_bytes(1))] {
            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
            let req = request(&data, format);
            validate_chat_input_audio(&req).expect("valid canonical audio input");
            validate_chat_media_inputs(&req).expect("combined media admission");
            let encoded = serde_json::to_value(req).unwrap();
            assert_eq!(
                encoded["messages"][0]["content"][0]["input_audio"]["format"],
                format
            );
        }

        assert_eq!(
            validate_chat_input_audio(&request("", "wav")),
            Err(InputAudioValidationError::EmptyData)
        );
        assert_eq!(
            validate_chat_input_audio(&request("not base64!", "wav")),
            Err(InputAudioValidationError::InvalidBase64)
        );
        assert_eq!(
            validate_chat_input_audio(&request("data:audio/wav;base64,UklGRg==", "wav")),
            Err(InputAudioValidationError::InvalidBase64)
        );
        assert_eq!(
            validate_chat_input_audio(&request("UklGRg==", "ogg")),
            Err(InputAudioValidationError::UnsupportedFormat)
        );
        assert_eq!(
            validate_chat_input_audio(&request("UklGRg==", "wav")),
            Err(InputAudioValidationError::FormatMismatch)
        );
        assert_eq!(
            validate_chat_input_audio(&request("SUQz", "wav")),
            Err(InputAudioValidationError::FormatMismatch)
        );
    }

    #[test]
    fn input_audio_enforces_container_count_and_aggregate_decoded_byte_limits() {
        validate_input_audio_bytes("wav", &wav_bytes(2, 1, 16_000, 16))
            .expect("metadata-complete WAV");
        validate_input_audio_bytes("mp3", &mp3_bytes(1)).expect("complete MPEG Layer III frame");
        assert_eq!(
            validate_input_audio_bytes("wav", b"ID3"),
            Err(InputAudioValidationError::FormatMismatch)
        );
        assert_eq!(
            validate_input_audio_bytes("wav", &vec![0; MAX_INLINE_AUDIO_BYTES + 1]),
            Err(InputAudioValidationError::DecodedBytesExceeded)
        );

        let request = |audio: Vec<InputAudio>| ChatCompletionRequest {
            messages: vec![Message::User {
                content: MessageContent::Parts(
                    audio
                        .into_iter()
                        .map(|input_audio| ContentPart::InputAudio { input_audio })
                        .collect(),
                ),
                name: None,
            }],
            ..Default::default()
        };
        let too_many = (0..=MAX_INPUT_AUDIO_PARTS_PER_REQUEST)
            .map(|_| InputAudio {
                data: base64::engine::general_purpose::STANDARD.encode(wav_bytes(2, 1, 16_000, 16)),
                format: "wav".into(),
            })
            .collect();
        assert_eq!(
            validate_chat_input_audio(&request(too_many)),
            Err(InputAudioValidationError::TooManyParts)
        );

        let per_audio_data_len = MAX_INLINE_AUDIO_BYTES / 2 + 1 - 44;
        let inline = || InputAudio {
            data: base64::engine::general_purpose::STANDARD.encode(wav_bytes(
                per_audio_data_len,
                8,
                192_000,
                64,
            )),
            format: "wav".into(),
        };
        assert_eq!(
            validate_chat_input_audio(&request(vec![inline(), inline()])),
            Err(InputAudioValidationError::DecodedBytesExceeded)
        );
    }

    #[test]
    fn input_audio_enforces_metadata_and_aggregate_duration_limits() {
        let malformed_wav = b"RIFF\x04\0\0\0WAVE";
        assert_eq!(
            validate_input_audio_bytes("wav", malformed_wav),
            Err(InputAudioValidationError::InvalidContainer)
        );
        assert_eq!(
            validate_input_audio_bytes("mp3", b"ID3\x04\0\0\0\0\0\0"),
            Err(InputAudioValidationError::InvalidContainer)
        );
        assert_eq!(
            validate_input_audio_bytes("wav", &wav_bytes(2, 1, 192_001, 16)),
            Err(InputAudioValidationError::SampleRateExceeded)
        );
        assert_eq!(
            validate_input_audio_bytes("wav", &wav_bytes(18, 9, 1, 16)),
            Err(InputAudioValidationError::ChannelCountExceeded)
        );
        assert_eq!(
            validate_input_audio_bytes(
                "wav",
                &wav_bytes(
                    usize::try_from(MAX_INPUT_AUDIO_DURATION_SECONDS + 1).unwrap(),
                    1,
                    1,
                    8,
                ),
            ),
            Err(InputAudioValidationError::DurationExceeded)
        );

        let request = |duration_seconds: usize| InputAudio {
            data: base64::engine::general_purpose::STANDARD.encode(wav_bytes(
                duration_seconds,
                1,
                1,
                8,
            )),
            format: "wav".into(),
        };
        let req = ChatCompletionRequest {
            messages: vec![Message::User {
                content: MessageContent::Parts(
                    vec![301, 300]
                        .into_iter()
                        .map(request)
                        .map(|input_audio| ContentPart::InputAudio { input_audio })
                        .collect(),
                ),
                name: None,
            }],
            ..Default::default()
        };
        assert_eq!(
            validate_chat_input_audio(&req),
            Err(InputAudioValidationError::DurationExceeded)
        );

        let frames_at_limit =
            usize::try_from(MAX_INPUT_AUDIO_DURATION_SECONDS * 44_100 / 1_152).unwrap();
        validate_input_audio_bytes("mp3", &mp3_bytes(frames_at_limit))
            .expect("bounded MP3 frame walk");
        assert_eq!(
            validate_input_audio_bytes("mp3", &mp3_bytes(frames_at_limit + 2)),
            Err(InputAudioValidationError::DurationExceeded)
        );
    }

    #[test]
    fn tool_choice_constructors_serialize_to_the_wire_form() {
        // The string variants stay an untagged bare string …
        assert_eq!(
            serde_json::to_value(ToolChoice::auto()).unwrap(),
            serde_json::json!("auto")
        );
        assert_eq!(
            serde_json::to_value(ToolChoice::none()).unwrap(),
            serde_json::json!("none")
        );
        assert_eq!(
            serde_json::to_value(ToolChoice::required()).unwrap(),
            serde_json::json!("required")
        );
        // … and `function(name)` produces the object form.
        assert_eq!(
            serde_json::to_value(ToolChoice::function("get_weather")).unwrap(),
            serde_json::json!({ "type": "function", "function": { "name": "get_weather" } })
        );
    }
}

#[cfg(test)]
mod document_part_tests {
    use super::embeddings_default_tests::{gif_bytes, png_bytes, webp_bytes};
    use super::*;

    /// The OpenAI `{"type":"file","file":{...}}` content-part shape deserializes
    /// into `ContentPart::Document`, splitting the `file_data` data-URL into a
    /// base64 source and carrying the filename.
    #[test]
    fn openai_file_part_deserializes_into_document() {
        let json = serde_json::json!({
            "type": "file",
            "file": {
                "filename": "draconomicon.pdf",
                "file_data": "data:application/pdf;base64,JVBERi0xLjQK"
            }
        });
        let part: ContentPart = serde_json::from_value(json).unwrap();
        let ContentPart::Document { document } = part else {
            panic!("expected ContentPart::Document, got {part:?}");
        };
        assert_eq!(document.filename.as_deref(), Some("draconomicon.pdf"));
        match document.source {
            DocumentSource::Base64 { media_type, data } => {
                assert_eq!(media_type, "application/pdf");
                assert_eq!(data, "JVBERi0xLjQK");
            }
            other => panic!("expected base64 source, got {other:?}"),
        }
    }

    /// The Anthropic `{"type":"document","source":{...}}` content-part shape
    /// deserializes into the SAME `ContentPart::Document` variant.
    #[test]
    fn anthropic_document_part_deserializes_into_document() {
        let json = serde_json::json!({
            "type": "document",
            "source": {
                "type": "base64",
                "media_type": "application/pdf",
                "data": "JVBERi0xLjQK"
            }
        });
        let part: ContentPart = serde_json::from_value(json).unwrap();
        let ContentPart::Document { document } = part else {
            panic!("expected ContentPart::Document, got {part:?}");
        };
        assert!(document.filename.is_none());
        match document.source {
            DocumentSource::Base64 { media_type, data } => {
                assert_eq!(media_type, "application/pdf");
                assert_eq!(data, "JVBERi0xLjQK");
            }
            other => panic!("expected base64 source, got {other:?}"),
        }
    }

    /// The Anthropic `url` document source round-trips too.
    #[test]
    fn anthropic_url_document_part_deserializes() {
        let json = serde_json::json!({
            "type": "document",
            "source": { "type": "url", "url": "https://example.com/report.pdf" }
        });
        let part: ContentPart = serde_json::from_value(json).unwrap();
        let ContentPart::Document { document } = part else {
            panic!("expected ContentPart::Document");
        };
        match document.source {
            DocumentSource::Url { url } => assert_eq!(url, "https://example.com/report.pdf"),
            other => panic!("expected url source, got {other:?}"),
        }
    }

    /// A parsed document part serializes to the canonical
    /// `{"type":"document","document":{"source":{...},"filename":...}}` form and
    /// that form round-trips back to an equal value.
    #[test]
    fn document_part_serializes_to_canonical_and_round_trips() {
        let json = serde_json::json!({
            "type": "file",
            "file": {
                "filename": "a.pdf",
                "file_data": "data:application/pdf;base64,QUJD"
            }
        });
        let part: ContentPart = serde_json::from_value(json).unwrap();
        let canonical = serde_json::to_value(&part).unwrap();
        assert_eq!(
            canonical,
            serde_json::json!({
                "type": "document",
                "document": {
                    "source": {
                        "type": "base64",
                        "media_type": "application/pdf",
                        "data": "QUJD"
                    },
                    "filename": "a.pdf"
                }
            })
        );
        // Canonical form re-deserializes into an equal Document part.
        let reparsed: ContentPart = serde_json::from_value(canonical).unwrap();
        let ContentPart::Document { document } = reparsed else {
            panic!("canonical form must re-parse as Document");
        };
        assert_eq!(document.filename.as_deref(), Some("a.pdf"));
    }

    #[test]
    fn document_sources_validate_closed_base64_https_data_url_and_file_id_shapes() {
        let request = |source: DocumentSource| ChatCompletionRequest {
            messages: vec![Message::User {
                content: MessageContent::Parts(vec![ContentPart::Document {
                    document: DocumentPart {
                        source,
                        filename: Some("report.pdf".into()),
                    },
                }]),
                name: None,
            }],
            ..Default::default()
        };

        for source in [
            DocumentSource::Base64 {
                media_type: "application/pdf".into(),
                data: "JVBERi0xLjQK".into(),
            },
            DocumentSource::Base64 {
                media_type: "image/tiff".into(),
                data: "SUkqAAgAAAACAAABBAABAAAAAQAAAAEBBAABAAAAAQAAAAAAAAA=".into(),
            },
            DocumentSource::Url {
                url: "https://objects.example.test/private/report.pdf?sig=abc".into(),
            },
            DocumentSource::Url {
                url: "data:application/pdf;base64,JVBERi0xLjQK".into(),
            },
            DocumentSource::Url {
                url: "file-abc_123-XYZ".into(),
            },
        ] {
            let req = request(source);
            validate_chat_documents(&req).expect("valid document source");
            validate_chat_media_inputs(&req).expect("combined media admission");
        }

        for (source, expected) in [
            (
                DocumentSource::Base64 {
                    media_type: "text/html".into(),
                    data: "PGgxPg==".into(),
                },
                DocumentValidationError::UnsupportedMediaType,
            ),
            (
                DocumentSource::Base64 {
                    media_type: "application/pdf".into(),
                    data: String::new(),
                },
                DocumentValidationError::EmptyData,
            ),
            (
                DocumentSource::Base64 {
                    media_type: "application/pdf".into(),
                    data: "not base64!".into(),
                },
                DocumentValidationError::InvalidBase64,
            ),
            (
                DocumentSource::Url {
                    url: "http://example.com/report.pdf".into(),
                },
                DocumentValidationError::InsecureUrl,
            ),
            (
                DocumentSource::Url {
                    url: "https://user:password@example.com/report.pdf".into(),
                },
                DocumentValidationError::EmbeddedCredentials,
            ),
            (
                DocumentSource::Url {
                    url: "data:application/pdf;base64,not base64!".into(),
                },
                DocumentValidationError::InvalidBase64,
            ),
            (
                DocumentSource::Url {
                    url: "file-bad!".into(),
                },
                DocumentValidationError::InvalidFileId,
            ),
            (
                DocumentSource::Url {
                    url: "not-a-url".into(),
                },
                DocumentValidationError::InvalidUrl,
            ),
        ] {
            assert_eq!(validate_chat_documents(&request(source)), Err(expected));
        }
    }

    #[test]
    fn document_byte_validation_checks_each_supported_container_signature() {
        let mut png = png_bytes(None);

        let jpeg = vec![
            0xff, 0xd8, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x01, 0x00, 0x01,
        ];
        let gif = gif_bytes(1);
        let webp = webp_bytes(None);

        let mut bmp = vec![0; 26];
        bmp[..2].copy_from_slice(b"BM");
        bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
        bmp[18..22].copy_from_slice(&1i32.to_le_bytes());
        bmp[22..26].copy_from_slice(&1i32.to_le_bytes());

        let tiff = |little_endian: bool| {
            let mut bytes = vec![0; 38];
            let u16_bytes = |value: u16| {
                if little_endian {
                    value.to_le_bytes()
                } else {
                    value.to_be_bytes()
                }
            };
            let u32_bytes = |value: u32| {
                if little_endian {
                    value.to_le_bytes()
                } else {
                    value.to_be_bytes()
                }
            };
            bytes[..2].copy_from_slice(if little_endian { b"II" } else { b"MM" });
            bytes[2..4].copy_from_slice(&u16_bytes(42));
            bytes[4..8].copy_from_slice(&u32_bytes(8));
            bytes[8..10].copy_from_slice(&u16_bytes(2));
            for (offset, tag) in [(10usize, 256u16), (22usize, 257u16)] {
                bytes[offset..offset + 2].copy_from_slice(&u16_bytes(tag));
                bytes[offset + 2..offset + 4].copy_from_slice(&u16_bytes(4));
                bytes[offset + 4..offset + 8].copy_from_slice(&u32_bytes(1));
                bytes[offset + 8..offset + 12].copy_from_slice(&u32_bytes(1));
            }
            bytes
        };
        let little_tiff = tiff(true);
        let big_tiff = tiff(false);

        for (media_type, bytes) in [
            ("application/pdf", b"prefix%PDF-1.7\n".as_slice()),
            ("image/png", png.as_slice()),
            ("image/jpeg", jpeg.as_slice()),
            ("image/gif", gif.as_slice()),
            ("image/webp", webp.as_slice()),
            ("image/bmp", bmp.as_slice()),
            ("image/tiff", little_tiff.as_slice()),
            ("image/tiff", big_tiff.as_slice()),
        ] {
            validate_document_bytes(media_type, bytes)
                .unwrap_or_else(|error| panic!("{media_type} signature rejected: {error}"));
        }

        assert_eq!(
            validate_document_bytes("application/pdf", b"not really a PDF"),
            Err(DocumentValidationError::MediaTypeMismatch)
        );
        assert_eq!(
            validate_document_bytes("image/png", b"%PDF-1.7"),
            Err(DocumentValidationError::MediaTypeMismatch)
        );
        assert_eq!(
            validate_document_bytes("image/png", b"\x89PNG\r\n\x1a\n"),
            Err(DocumentValidationError::InvalidImageDimensions)
        );
        png[16..20].copy_from_slice(&(MAX_INLINE_IMAGE_DIMENSION + 1).to_be_bytes());
        assert_eq!(
            validate_document_bytes("image/png", &png),
            Err(DocumentValidationError::ImageDimensionsExceeded)
        );
        assert_eq!(
            validate_document_bytes(
                "application/pdf",
                &vec![b'x'; MAX_INLINE_DOCUMENT_BYTES + 1]
            ),
            Err(DocumentValidationError::DecodedBytesExceeded)
        );
    }

    #[test]
    fn document_request_enforces_count_and_aggregate_decoded_byte_limits() {
        let request = |sources: Vec<DocumentSource>| ChatCompletionRequest {
            messages: vec![Message::User {
                content: MessageContent::Parts(
                    sources
                        .into_iter()
                        .map(|source| ContentPart::Document {
                            document: DocumentPart {
                                source,
                                filename: Some("report.pdf".into()),
                            },
                        })
                        .collect(),
                ),
                name: None,
            }],
            ..Default::default()
        };

        let too_many = (0..=MAX_DOCUMENT_PARTS_PER_REQUEST)
            .map(|index| DocumentSource::Url {
                url: format!("https://example.com/document-{index}.pdf"),
            })
            .collect();
        assert_eq!(
            validate_chat_documents(&request(too_many)),
            Err(DocumentValidationError::TooManyDocuments)
        );

        let per_document_len = MAX_INLINE_DOCUMENT_BYTES / 2 + 1;
        let inline = || {
            let mut bytes = vec![0; per_document_len];
            bytes[..5].copy_from_slice(b"%PDF-");
            DocumentSource::Base64 {
                media_type: "application/pdf".into(),
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            }
        };
        assert_eq!(
            validate_chat_documents(&request(vec![inline(), inline()])),
            Err(DocumentValidationError::DecodedBytesExceeded)
        );
    }

    #[test]
    fn openai_file_id_deserializes_and_validates_as_an_opaque_source() {
        let part: ContentPart = serde_json::from_value(serde_json::json!({
            "type": "file",
            "file": {
                "file_id": "file-abc_123-XYZ",
                "filename": "report.pdf"
            }
        }))
        .unwrap();
        let ContentPart::Document { document } = part else {
            panic!("expected a canonical document part");
        };
        document.validate().expect("valid opaque OpenAI file id");
        assert!(matches!(
            document.source,
            DocumentSource::Url { ref url } if url == "file-abc_123-XYZ"
        ));
    }

    /// Text and image parts are unaffected by the new variant.
    #[test]
    fn text_and_image_parts_unaffected() {
        let text: ContentPart = serde_json::from_value(serde_json::json!({
            "type": "text", "text": "hello"
        }))
        .unwrap();
        assert!(matches!(text, ContentPart::Text { .. }));

        let image: ContentPart = serde_json::from_value(serde_json::json!({
            "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" }
        }))
        .unwrap();
        assert!(matches!(image, ContentPart::ImageUrl { .. }));
    }
}
