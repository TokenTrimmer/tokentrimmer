//! Delta/diff responses (research Phase 3.4): opt-in anchored search/replace
//! patches for edit/iteration routes, with fail-closed full re-emit.
//!
//! ## The prior seam (honest, stateless)
//!
//! The gateway is stateless per-request, so the PRIOR ARTIFACT must come from
//! the request itself — there is NO server-side session state (that is the
//! #451 substrate and out of scope). Two sources, explicit beats inferred:
//!
//! 1. `tt_extras["diff_prior"]` — an explicit opt-in echo of the current
//!    document (string). `tt_extras` is a TokenTrimmer-internal side channel:
//!    every adapter strips it before upstream dispatch, and
//!    [`apply_diff_request`] removes the key as well (defense in depth), so
//!    the echoed document never rides to the provider twice.
//! 2. Else the LAST `Message::Assistant` text in the request history — the
//!    common edit-loop shape: `[user ask][assistant doc][user "change X"]`.
//!
//! No identifiable prior (or one under [`MIN_PRIOR_CHARS`], where patch
//! overhead exceeds any saving) ⇒ no-op with a warning.
//!
//! ## The patch wire grammar (v1: anchored search/replace ONLY)
//!
//! ```text
//! <<<<<<< SEARCH
//! (verbatim unique excerpt of the current document)
//! =======
//! (replacement text)
//! >>>>>>> REPLACE
//! ```
//!
//! Multiple blocks allowed, applied sequentially against the running result;
//! each SEARCH must match EXACTLY ONCE. RFC-6902 JSON-patch is a documented
//! follow-up (needs a dependency); JSON artifacts are still covered because
//! they are text and [`reconstruct`] additionally requires the result to
//! parse as JSON whenever the caller set a JSON `response_format`.
//!
//! KNOWN GRAMMAR LIMIT (v1, fails SAFE): the markers are matched on the
//! trimmed content of every line, including lines inside a block body, so a
//! document whose anchor region itself contains a line trimming to
//! `=======` (e.g. a 7-char markdown setext underline) cannot be quoted in
//! any valid patch — every edit touching such a region fails CLOSED and
//! pays the honest re-emit retry tax. Routes serving such documents should
//! not enable `diff`; a marker-escaping scheme is a documented follow-up.
//!
//! ## Fail-closed
//!
//! ANY validation failure (truncated emission / no blocks / malformed /
//! missing or ambiguous anchor / result not JSON when required) ⇒ the
//! handler re-dispatches a full re-emit and books the failed patch
//! attempt's realized cost honestly (`diff_failed` +
//! `diff_failed_cost_usd`). The re-emit request is the DISPATCHED request
//! with this module's mutation inverted ([`unapply_diff_request`]) so it
//! inherits redaction/compression/flex — never a pre-pipeline clone, which
//! would leak unredacted content upstream on a `redact`+`diff` route.
//! OpenAI Predicted Outputs is deliberately NOT used anywhere
//! (latency-only, no billing discount), and `max_tokens` is never set or
//! modified (Anthropic counts thinking inside it).

use tt_shared::messages::{Message, MessageContent, ResponseFormat};
use tt_shared::ChatCompletionRequest;

use super::ShapeDecision;

/// `tt_extras` key carrying the explicit prior-artifact echo (string value).
pub(crate) const TT_EXTRA_DIFF_PRIOR: &str = "diff_prior";

/// Priors below this length skip: the patch grammar + instruction overhead
/// exceeds any plausible saving on a small document.
pub(crate) const MIN_PRIOR_CHARS: usize = 200;

/// The deterministic patch instruction appended as a trailing System message.
/// Byte-exact contract — tests assert on it.
pub(crate) const DIFF_INSTRUCTION: &str = "You are editing the most recent \
version of the document above. Respond ONLY with an anchored search/replace \
patch — do NOT re-emit the document. Use one or more blocks of EXACTLY this \
form:\n<<<<<<< SEARCH\n(verbatim excerpt of the current document, long enough \
to be unique)\n=======\n(replacement text)\n>>>>>>> REPLACE\nEach SEARCH \
excerpt must appear EXACTLY ONCE in the current document. Keep excerpts \
minimal. No prose outside the blocks.";

/// Where the prior artifact came from (telemetry/debug only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PriorSource {
    /// Explicit `tt_extras["diff_prior"]` echo.
    Extras,
    /// Inferred from the last assistant message in the request history.
    AssistantHistory,
}

/// A validated decision to request a patch instead of a full re-emission.
#[derive(Debug, Clone)]
pub(crate) struct DiffPlan {
    /// The prior artifact the patch applies against.
    pub prior: String,
    #[allow(dead_code)] // telemetry/debug seam; read by tests
    pub source: PriorSource,
    /// Caller's `response_format`, retained for post-apply validation and
    /// restored onto the fail-closed re-emit ([`unapply_diff_request`]); the
    /// upstream patch request drops it (the patch is not the artifact).
    pub response_format: Option<ResponseFormat>,
}

/// PURE planner. `None` when the route did not opt in. Gates (first failure
/// wins): streaming, tools, n>1, strict json_schema (we would have to drop
/// the grammar lock to receive a patch and could only hand-validate
/// JSON-parses, not schema conformance — too weak, so skip), no identifiable
/// prior, prior too small.
pub(crate) fn plan_diff(
    req: &ChatCompletionRequest,
    requested: bool,
) -> Option<ShapeDecision<DiffPlan>> {
    if !requested {
        return None;
    }
    if req.stream {
        return Some(ShapeDecision::Skip("streaming"));
    }
    if !req.tools.is_empty() || req.tool_choice.is_some() {
        return Some(ShapeDecision::Skip("tools"));
    }
    if req.n.is_some_and(|n| n > 1) {
        return Some(ShapeDecision::Skip("n"));
    }
    if is_strict_json_schema(req.response_format.as_ref()) {
        return Some(ShapeDecision::Skip("strict_schema"));
    }
    // An explicit echo of the WRONG TYPE (e.g. the caller sent the JSON
    // artifact itself instead of a string) gets its own skip — falling back
    // to the assistant history would silently anchor the patch against a
    // different document than the one the caller explicitly supplied.
    if req
        .tt_extras
        .get(TT_EXTRA_DIFF_PRIOR)
        .is_some_and(|v| !v.is_string())
    {
        return Some(ShapeDecision::Skip("bad_prior_type"));
    }
    let Some((prior, source)) = identify_prior(req) else {
        return Some(ShapeDecision::Skip("no_prior"));
    };
    if prior.len() < MIN_PRIOR_CHARS {
        return Some(ShapeDecision::Skip("prior_too_small"));
    }
    Some(ShapeDecision::Apply(DiffPlan {
        prior,
        source,
        response_format: req.response_format.clone(),
    }))
}

fn is_strict_json_schema(rf: Option<&ResponseFormat>) -> bool {
    let Some(rf) = rf else { return false };
    if rf.r#type != "json_schema" {
        return false;
    }
    rf.json_schema
        .as_ref()
        .and_then(|s| s.get("strict"))
        .and_then(|s| s.as_bool())
        .unwrap_or(false)
}

/// The prior seam: explicit `tt_extras["diff_prior"]` echo beats the last
/// assistant message in the history. `None` when neither exists. A PRESENT
/// but non-string echo never reaches here — `plan_diff` skips it with
/// `bad_prior_type` instead of silently inferring a different prior than the
/// one the caller explicitly supplied.
fn identify_prior(req: &ChatCompletionRequest) -> Option<(String, PriorSource)> {
    if let Some(v) = req.tt_extras.get(TT_EXTRA_DIFF_PRIOR) {
        if let Some(s) = v.as_str() {
            return Some((s.to_string(), PriorSource::Extras));
        }
    }
    let last_assistant = req.messages.iter().rev().find_map(|m| match m {
        Message::Assistant {
            content: Some(content),
            ..
        } => {
            let text = match content {
                MessageContent::Text(s) => s.clone(),
                MessageContent::Parts(parts) => parts
                    .iter()
                    .filter_map(|p| match p {
                        tt_shared::messages::ContentPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            };
            if text.trim().is_empty() {
                None
            } else {
                Some(text)
            }
        }
        _ => None,
    })?;
    Some((last_assistant, PriorSource::AssistantHistory))
}

/// Mutate the request for the PATCH dispatch: drop `response_format` (the
/// patch is not the artifact — a JSON grammar would fight the patch wire
/// form), remove the consumed `tt_extras` echo (defense in depth — adapters
/// strip `tt_extras` anyway), and append [`DIFF_INSTRUCTION`] as a trailing
/// System message. `max_tokens` is NEVER set or modified. The fail-closed
/// re-emit inverts this mutation on the dispatched request via
/// [`unapply_diff_request`] — never a pre-pipeline clone (see there).
pub(crate) fn apply_diff_request(req: &mut ChatCompletionRequest, _plan: &DiffPlan) {
    req.response_format = None;
    req.tt_extras.remove(TT_EXTRA_DIFF_PRIOR);
    req.messages.push(Message::System {
        content: MessageContent::Text(DIFF_INSTRUCTION.to_string()),
    });
}

/// Invert [`apply_diff_request`] on a clone of the DISPATCHED request to
/// build the fail-closed re-emit: remove the trailing patch-instruction
/// System message (matched byte-exactly from the end — the constant is never
/// touched by the request passes) and restore the caller's `response_format`
/// from the plan (the re-emit IS the artifact, so the caller's contract
/// applies again).
///
/// The re-emit is derived from the dispatched request — NOT from a
/// pre-pipeline clone — so it inherits every dispatch-path normalization the
/// patch dispatch carried: the redaction guardrail (a `redact`+`diff` route's
/// re-emit must never out-leak the dispatch path), compression, the flex
/// tier (`compute_cost_full` prices the metered re-emit with `flex_applied`),
/// and the temperature clamp. The caller re-runs the provider
/// `response_format` downgrade on the restored value before dispatching.
pub(crate) fn unapply_diff_request(req: &mut ChatCompletionRequest, plan: &DiffPlan) {
    if let Some(pos) = req.messages.iter().rposition(|m| {
        matches!(
            m,
            Message::System {
                content: MessageContent::Text(t),
            } if t == DIFF_INSTRUCTION
        )
    }) {
        req.messages.remove(pos);
    }
    req.response_format = plan.response_format.clone();
}

/// One anchored search/replace block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SrBlock {
    pub search: String,
    pub replace: String,
}

/// Why a patch failed to validate/apply — every variant fails CLOSED to a
/// full re-emit at the handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffError {
    /// The response contained no SEARCH/REPLACE blocks at all (e.g. the
    /// model re-emitted the document, emitted prose, or emitted only tool
    /// calls).
    NoBlocks,
    /// Block structure violated (unterminated block, stray marker, empty
    /// SEARCH anchor).
    Malformed,
    /// A SEARCH anchor matched nothing in the running document.
    AnchorMissing,
    /// A SEARCH anchor matched more than once — applying would be a guess.
    AnchorAmbiguous,
    /// The reconstructed artifact must parse as JSON (caller set a JSON
    /// `response_format`) but does not.
    InvalidJson,
    /// The patch emission was cut off by the provider (`finish_reason` other
    /// than `"stop"`, e.g. a `max_tokens`/`length` truncation). Checked
    /// BEFORE parsing: a truncation that happens to land exactly on a
    /// `>>>>>>> REPLACE` boundary parses as a valid-but-incomplete
    /// multi-block patch and would silently serve a PARTIALLY edited
    /// artifact — the one corruption the structural checks cannot see.
    Truncated,
}

impl DiffError {
    /// Bounded warnings-token / metric-label suffix.
    pub fn reason(&self) -> &'static str {
        match self {
            DiffError::NoBlocks => "no_blocks",
            DiffError::Malformed => "malformed_patch",
            DiffError::AnchorMissing => "anchor_missing",
            DiffError::AnchorAmbiguous => "anchor_ambiguous",
            DiffError::InvalidJson => "invalid_json",
            DiffError::Truncated => "truncated",
        }
    }
}

const SEARCH_MARKER: &str = "<<<<<<< SEARCH";
const DIVIDER_MARKER: &str = "=======";
const REPLACE_MARKER: &str = ">>>>>>> REPLACE";

/// Parse the model's patch emission into blocks. Whole-patch markdown fences
/// are tolerated (dropped); stray prose OUTSIDE blocks is ignored (the
/// instruction forbids it, but a preamble must not turn a valid patch into a
/// failed dispatch); a stray divider/terminator outside a block, an
/// unterminated block, or an empty SEARCH anchor is [`DiffError::Malformed`];
/// zero blocks is [`DiffError::NoBlocks`].
pub(crate) fn parse_search_replace(patch: &str) -> Result<Vec<SrBlock>, DiffError> {
    #[derive(PartialEq)]
    enum State {
        Outside,
        InSearch,
        InReplace,
    }
    let mut state = State::Outside;
    let mut blocks = Vec::new();
    let mut search: Vec<&str> = Vec::new();
    let mut replace: Vec<&str> = Vec::new();
    for line in patch.lines() {
        let trimmed = line.trim_end();
        match state {
            State::Outside => match trimmed.trim() {
                t if t == SEARCH_MARKER => state = State::InSearch,
                t if t == DIVIDER_MARKER || t == REPLACE_MARKER => {
                    return Err(DiffError::Malformed);
                }
                t if t.starts_with("```") => {} // whole-patch fence
                _ => {}                         // prose outside blocks: tolerated, ignored
            },
            State::InSearch => match trimmed.trim() {
                t if t == DIVIDER_MARKER => state = State::InReplace,
                t if t == SEARCH_MARKER || t == REPLACE_MARKER => {
                    return Err(DiffError::Malformed);
                }
                _ => search.push(line),
            },
            State::InReplace => match trimmed.trim() {
                t if t == REPLACE_MARKER => {
                    let s = search.join("\n");
                    if s.is_empty() {
                        // An empty anchor would match everywhere.
                        return Err(DiffError::Malformed);
                    }
                    blocks.push(SrBlock {
                        search: s,
                        replace: replace.join("\n"),
                    });
                    search.clear();
                    replace.clear();
                    state = State::Outside;
                }
                t if t == SEARCH_MARKER || t == DIVIDER_MARKER => {
                    return Err(DiffError::Malformed);
                }
                _ => replace.push(line),
            },
        }
    }
    if state != State::Outside {
        return Err(DiffError::Malformed); // unterminated block
    }
    if blocks.is_empty() {
        return Err(DiffError::NoBlocks);
    }
    Ok(blocks)
}

/// Apply blocks sequentially against the running result. Each SEARCH must
/// occur EXACTLY ONCE in the current text (zero matches ⇒
/// [`DiffError::AnchorMissing`], more than one ⇒
/// [`DiffError::AnchorAmbiguous`]) — anything else would be a silent guess
/// about the caller's document. The second-occurrence scan is
/// OVERLAP-AWARE: `str::matches` counts only disjoint occurrences, so a
/// self-overlapping anchor (`"aa"` in `"aaa"` — repeated-pattern text such
/// as separator runs or duplicated lines) would otherwise count 1 and apply
/// as a silent guess at the first position.
pub(crate) fn apply_patch(prior: &str, blocks: &[SrBlock]) -> Result<String, DiffError> {
    let mut current = prior.to_string();
    for block in blocks {
        let search = block.search.as_str();
        let Some(first) = current.find(search) else {
            return Err(DiffError::AnchorMissing);
        };
        // Rescan from one character past the first match start (the anchor
        // is non-empty, so this is a char boundary inside/at the end of the
        // match) — catches overlapping AND disjoint second occurrences.
        let step = search.chars().next().map_or(1, char::len_utf8);
        if current[first + step..].contains(search) {
            return Err(DiffError::AnchorAmbiguous);
        }
        current.replace_range(first..first + search.len(), &block.replace);
    }
    Ok(current)
}

/// parse → apply → (when the caller's `response_format` is
/// `json_object`/`json_schema`: the result must parse as JSON) → the FULL
/// reconstructed artifact the caller receives.
pub(crate) fn reconstruct(plan: &DiffPlan, patch_text: &str) -> Result<String, DiffError> {
    let blocks = parse_search_replace(patch_text)?;
    let artifact = apply_patch(&plan.prior, &blocks)?;
    let wants_json = plan
        .response_format
        .as_ref()
        .is_some_and(|rf| rf.r#type == "json_object" || rf.r#type == "json_schema");
    if wants_json && serde_json::from_str::<serde_json::Value>(&artifact).is_err() {
        return Err(DiffError::InvalidJson);
    }
    Ok(artifact)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_doc() -> String {
        let mut s = String::from("# Design Doc\n\n");
        for i in 0..10 {
            s.push_str(&format!(
                "Section {i}: the quick brown fox jumps over the lazy dog.\n"
            ));
        }
        s
    }

    fn edit_loop_req(doc: &str) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![
                Message::User {
                    content: MessageContent::Text("write a design doc".into()),
                    name: None,
                },
                Message::Assistant {
                    content: Some(MessageContent::Text(doc.to_string())),
                    tool_calls: vec![],
                    name: None,
                },
                Message::User {
                    content: MessageContent::Text("rename Section 3".into()),
                    name: None,
                },
            ],
            ..Default::default()
        }
    }

    fn applied(d: Option<ShapeDecision<DiffPlan>>) -> DiffPlan {
        match d {
            Some(ShapeDecision::Apply(p)) => p,
            Some(ShapeDecision::Skip(r)) => panic!("expected Apply, got Skip({r})"),
            None => panic!("expected Apply, got None"),
        }
    }

    fn skip_reason(d: Option<ShapeDecision<DiffPlan>>) -> &'static str {
        match d {
            Some(ShapeDecision::Skip(r)) => r,
            _ => panic!("expected Skip"),
        }
    }

    /// Not requested → None (zero work on the default path).
    #[test]
    fn not_requested_is_none() {
        assert!(plan_diff(&edit_loop_req(&long_doc()), false).is_none());
    }

    /// Prior precedence: the explicit tt_extras echo beats the last assistant
    /// message; neither → no_prior; a short prior → prior_too_small; strict
    /// json_schema → strict_schema.
    #[test]
    fn prior_seam_matrix() {
        // Last assistant message (the common edit-loop shape).
        let doc = long_doc();
        let plan = applied(plan_diff(&edit_loop_req(&doc), true));
        assert_eq!(plan.prior, doc);
        assert_eq!(plan.source, PriorSource::AssistantHistory);

        // Explicit echo wins over the history.
        let echoed = "E".repeat(MIN_PRIOR_CHARS + 1);
        let mut req = edit_loop_req(&doc);
        req.tt_extras.insert(
            TT_EXTRA_DIFF_PRIOR.into(),
            serde_json::Value::String(echoed.clone()),
        );
        let plan = applied(plan_diff(&req, true));
        assert_eq!(plan.prior, echoed);
        assert_eq!(plan.source, PriorSource::Extras);

        // No prior at all.
        let bare = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![Message::User {
                content: MessageContent::Text("edit the doc".into()),
                name: None,
            }],
            ..Default::default()
        };
        assert_eq!(skip_reason(plan_diff(&bare, true)), "no_prior");

        // An explicit echo of the WRONG TYPE (object/array instead of a
        // string) skips with its own reason — never a silent fallback to the
        // assistant history (a different document than the caller supplied).
        let mut typed = edit_loop_req(&doc);
        typed.tt_extras.insert(
            TT_EXTRA_DIFF_PRIOR.into(),
            serde_json::json!({"title": "doc"}),
        );
        assert_eq!(skip_reason(plan_diff(&typed, true)), "bad_prior_type");

        // Prior under the floor.
        let small = edit_loop_req("tiny doc");
        assert_eq!(skip_reason(plan_diff(&small, true)), "prior_too_small");

        // Strict structured output disables the lane.
        let mut strict = edit_loop_req(&doc);
        strict.response_format = Some(ResponseFormat {
            r#type: "json_schema".into(),
            json_schema: Some(serde_json::json!({
                "name": "out", "strict": true, "schema": {"type": "object"}
            })),
        });
        assert_eq!(skip_reason(plan_diff(&strict, true)), "strict_schema");

        // Streaming / tools / n>1 are hard-ineligible.
        let mut streaming = edit_loop_req(&doc);
        streaming.stream = true;
        assert_eq!(skip_reason(plan_diff(&streaming, true)), "streaming");
        let mut tooled = edit_loop_req(&doc);
        tooled.tool_choice = Some(tt_shared::messages::ToolChoice::auto());
        assert_eq!(skip_reason(plan_diff(&tooled, true)), "tools");
        let mut multi = edit_loop_req(&doc);
        multi.n = Some(2);
        assert_eq!(skip_reason(plan_diff(&multi, true)), "n");
    }

    /// Mutation: response_format removed + instruction appended + the
    /// consumed tt_extras echo dropped; the PLAN retains the caller's
    /// response_format (used by `reconstruct`'s JSON gate and restored onto
    /// the re-emit by `unapply_diff_request`).
    #[test]
    fn apply_mutation_and_pre_diff_clone() {
        let doc = long_doc();
        let mut req = edit_loop_req(&doc);
        req.response_format = Some(ResponseFormat {
            r#type: "json_object".into(),
            json_schema: None,
        });
        req.tt_extras.insert(
            TT_EXTRA_DIFF_PRIOR.into(),
            serde_json::Value::String(doc.clone()),
        );
        let plan = applied(plan_diff(&req, true));
        let pre_diff = req.clone();
        apply_diff_request(&mut req, &plan);

        // The plan retained the caller's response_format for post-apply
        // validation; the dispatched request dropped it.
        assert_eq!(
            plan.response_format.as_ref().map(|rf| rf.r#type.as_str()),
            Some("json_object")
        );
        assert!(req.response_format.is_none());
        assert!(!req.tt_extras.contains_key(TT_EXTRA_DIFF_PRIOR));
        match req.messages.last().unwrap() {
            Message::System {
                content: MessageContent::Text(t),
            } => assert_eq!(t, DIFF_INSTRUCTION),
            other => panic!("expected trailing System instruction, got {other:?}"),
        }
        // A pre-mutation clone is untouched by the apply (no aliasing).
        assert_eq!(
            pre_diff
                .response_format
                .as_ref()
                .map(|rf| rf.r#type.as_str()),
            Some("json_object")
        );
        assert_eq!(pre_diff.messages.len(), req.messages.len() - 1);
        // max_tokens is NEVER set or modified (Anthropic counts thinking
        // inside it).
        assert_eq!(req.max_tokens, None);
    }

    /// unapply_diff_request inverts the mutation on the DISPATCHED request:
    /// the trailing instruction is removed and the caller's response_format
    /// restored — while every OTHER dispatch-path mutation (e.g. redaction
    /// of message content) survives on the re-emit.
    #[test]
    fn unapply_inverts_on_dispatched_request_preserving_pipeline_mutations() {
        let doc = long_doc();
        let mut req = edit_loop_req(&doc);
        req.response_format = Some(ResponseFormat {
            r#type: "json_object".into(),
            json_schema: None,
        });
        let plan = applied(plan_diff(&req, true));
        apply_diff_request(&mut req, &plan);

        // Simulate a post-instruction pipeline mutation (what the redaction
        // guardrail does to the dispatched bytes).
        if let Message::User { content, .. } = &mut req.messages[2] {
            *content = MessageContent::Text("rename [REDACTED]".into());
        }

        let mut reemit = req.clone();
        unapply_diff_request(&mut reemit, &plan);

        // Instruction gone, response_format restored.
        assert_eq!(reemit.messages.len(), req.messages.len() - 1);
        assert!(!reemit.messages.iter().any(|m| matches!(
            m,
            Message::System { content: MessageContent::Text(t) } if t == DIFF_INSTRUCTION
        )));
        assert_eq!(
            reemit.response_format.as_ref().map(|rf| rf.r#type.as_str()),
            Some("json_object")
        );
        // The pipeline mutation SURVIVES — the re-emit inherits the
        // dispatched (redacted) bytes, never the pre-pipeline original.
        match &reemit.messages[2] {
            Message::User {
                content: MessageContent::Text(t),
                ..
            } => assert_eq!(t, "rename [REDACTED]"),
            other => panic!("unexpected message shape: {other:?}"),
        }
        // Idempotent when no instruction is present (theoretical safety).
        let before = serde_json::to_string(&reemit).unwrap();
        unapply_diff_request(&mut reemit, &plan);
        assert_eq!(serde_json::to_string(&reemit).unwrap(), before);
    }

    /// Parser: single block, multiple blocks, whole-patch fences tolerated,
    /// malformed structures rejected, empty patch → NoBlocks.
    #[test]
    fn parse_search_replace_matrix() {
        let single = "<<<<<<< SEARCH\nold line\n=======\nnew line\n>>>>>>> REPLACE";
        assert_eq!(
            parse_search_replace(single).unwrap(),
            vec![SrBlock {
                search: "old line".into(),
                replace: "new line".into()
            }]
        );

        let multi = "<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE\n\
                     <<<<<<< SEARCH\nc\n=======\n\n>>>>>>> REPLACE";
        let blocks = parse_search_replace(multi).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1].replace, "");

        // Whole-patch fences + a prose preamble are tolerated.
        let fenced = "Here is the patch:\n```\n<<<<<<< SEARCH\nx\n=======\ny\n>>>>>>> REPLACE\n```";
        assert_eq!(parse_search_replace(fenced).unwrap().len(), 1);

        // Unterminated block.
        assert_eq!(
            parse_search_replace("<<<<<<< SEARCH\nx\n=======\ny"),
            Err(DiffError::Malformed)
        );
        // Stray divider outside any block.
        assert_eq!(
            parse_search_replace("=======\nx"),
            Err(DiffError::Malformed)
        );
        // Empty SEARCH anchor.
        assert_eq!(
            parse_search_replace("<<<<<<< SEARCH\n=======\ny\n>>>>>>> REPLACE"),
            Err(DiffError::Malformed)
        );
        // No blocks at all (prose / full re-emission / empty).
        assert_eq!(parse_search_replace(""), Err(DiffError::NoBlocks));
        assert_eq!(
            parse_search_replace("I cannot produce a patch."),
            Err(DiffError::NoBlocks)
        );
    }

    /// apply_patch: unique anchor replaces; zero matches → AnchorMissing; two
    /// matches → AnchorAmbiguous; multi-block applies sequentially against
    /// the RUNNING result.
    #[test]
    fn apply_patch_matrix() {
        let prior = "alpha\nbeta\ngamma";
        let ok = apply_patch(
            prior,
            &[SrBlock {
                search: "beta".into(),
                replace: "BETA".into(),
            }],
        )
        .unwrap();
        assert_eq!(ok, "alpha\nBETA\ngamma");

        assert_eq!(
            apply_patch(
                prior,
                &[SrBlock {
                    search: "delta".into(),
                    replace: "x".into()
                }]
            ),
            Err(DiffError::AnchorMissing)
        );

        assert_eq!(
            apply_patch(
                "dup\ndup",
                &[SrBlock {
                    search: "dup".into(),
                    replace: "x".into()
                }]
            ),
            Err(DiffError::AnchorAmbiguous)
        );

        // OVERLAPPING occurrences are ambiguous too: "aa" occurs twice in
        // "aaa" (positions 0 and 1) even though str::matches counts only 1
        // disjoint occurrence — applying would be a silent guess.
        assert_eq!(
            apply_patch(
                "aaa",
                &[SrBlock {
                    search: "aa".into(),
                    replace: "X".into()
                }]
            ),
            Err(DiffError::AnchorAmbiguous)
        );
        // Same with a multi-byte leading char (char-boundary safety of the
        // overlap rescan).
        assert_eq!(
            apply_patch(
                "ééé",
                &[SrBlock {
                    search: "éé".into(),
                    replace: "X".into()
                }]
            ),
            Err(DiffError::AnchorAmbiguous)
        );
        // A genuinely unique multi-byte anchor still applies cleanly.
        assert_eq!(
            apply_patch(
                "héllo wörld",
                &[SrBlock {
                    search: "wörld".into(),
                    replace: "world".into()
                }]
            )
            .unwrap(),
            "héllo world"
        );

        // Sequential semantics: block 2's anchor exists only AFTER block 1
        // ran.
        let seq = apply_patch(
            "one two",
            &[
                SrBlock {
                    search: "one".into(),
                    replace: "1".into(),
                },
                SrBlock {
                    search: "1 two".into(),
                    replace: "1 2".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(seq, "1 2");
    }

    /// reconstruct: a patched JSON prior that no longer parses (with a JSON
    /// response_format retained) → InvalidJson; a valid result passes and
    /// returns the FULL artifact.
    #[test]
    fn reconstruct_json_gate() {
        let prior = format!(
            "{{\"title\": \"doc\", \"pad\": \"{}\"}}",
            "x".repeat(MIN_PRIOR_CHARS)
        );
        let plan = DiffPlan {
            prior: prior.clone(),
            source: PriorSource::Extras,
            response_format: Some(ResponseFormat {
                r#type: "json_object".into(),
                json_schema: None,
            }),
        };

        // Break the JSON: drop the closing brace.
        let bad = "<<<<<<< SEARCH\n\"}\n=======\n\"\n>>>>>>> REPLACE";
        assert_eq!(reconstruct(&plan, bad), Err(DiffError::InvalidJson));

        // A clean edit passes and yields the full artifact.
        let good =
            "<<<<<<< SEARCH\n\"title\": \"doc\"\n=======\n\"title\": \"doc v2\"\n>>>>>>> REPLACE";
        let artifact = reconstruct(&plan, good).unwrap();
        assert!(artifact.contains("doc v2"));
        serde_json::from_str::<serde_json::Value>(&artifact).unwrap();

        // Without a JSON response_format the same broken result is accepted
        // (text artifact — nothing to parse).
        let text_plan = DiffPlan {
            response_format: None,
            ..plan
        };
        assert!(reconstruct(&text_plan, bad).is_ok());
    }
}
