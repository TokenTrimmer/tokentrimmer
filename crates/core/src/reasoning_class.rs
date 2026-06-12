//! Conservative reasoning-work classifier (research Phase 3.2).
//!
//! Classes where reasoning IS the work — capping reasoning spend there yields
//! confidently-wrong answers, so the reasoning-cap route action
//! (`RouteAction::reasoning_max_effort` / `reasoning_budget_tokens`) refuses
//! to act on them (`reasoning_cap_skipped:class:<c>`).
//!
//! Deliberately NOT the existing task-class machinery: `tt_cache::l2::TaskClass`
//! and `JudgeTaskClass` are single-variant INGRESS classes (chat-completions),
//! and `tt-preview`'s keyword classifier carries the wrong classes
//! (classification/extraction/code/agent/chat). This module is a content
//! classifier for exactly the four reasoning-is-the-work classes, pattern-
//! matched on the `tt-preview` classifier style: deterministic, lowercased
//! substring checks, no regex backtracking.
//!
//! Bias direction (READ THIS): OVER-matching (skipping a cap that would have
//! been fine) is safe — the request just spends what it would have spent.
//! UNDER-matching is not (a capped math proof comes back confidently wrong).
//! So the keyword sets err broad, and an unclassified request is cappable
//! only because the whole feature is off-by-default, route-gated, judge-
//! gateable, and auto-pause-composable — the safety net is layered.

/// Classes where reasoning IS the work — capping yields confidently-wrong
/// answers (research Phase 3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningWorkClass {
    Math,
    Code,
    Legal,
    Medical,
}

impl ReasoningWorkClass {
    /// Stable lowercase token for the warnings header / metric label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningWorkClass::Math => "math",
            ReasoningWorkClass::Code => "code",
            ReasoningWorkClass::Legal => "legal",
            ReasoningWorkClass::Medical => "medical",
        }
    }
}

/// Keyword sets, checked as case-folded substrings of the request's combined
/// text. Stems ("indemn", "diagnos", "prescri") deliberately match the whole
/// word family.
const MATH_KEYWORDS: &[&str] = &[
    "prove",
    "theorem",
    "lemma",
    "solve for",
    "integral",
    "derivative",
    "differential equation",
    "equation",
    "calculate the",
    "probability of",
    "combinatori",
    "algebra",
    "geometry proof",
];
const CODE_KEYWORDS: &[&str] = &[
    "```",
    "function",
    "implement",
    "refactor",
    "debug",
    "stack trace",
    "traceback",
    "compile",
    "unit test",
    "segfault",
    "null pointer",
    "regex",
    "source code",
];
const LEGAL_KEYWORDS: &[&str] = &[
    "contract",
    "liability",
    "statute",
    "clause",
    "indemn",
    "jurisdiction",
    "compliance with",
    "plaintiff",
    "defendant",
    "tort",
    "subpoena",
];
const MEDICAL_KEYWORDS: &[&str] = &[
    "diagnos",
    "dosage",
    "symptom",
    "patient",
    "contraindic",
    "prescri",
    "treatment plan",
    "differential diagnosis",
    "mg/kg",
    "clinical",
];

/// `Some(class)` when the request matches a reasoning-is-the-work class
/// (first match in Math → Code → Legal → Medical order); `None` = cappable.
///
/// The caller passes the ALREADY-lowercased combined request text (typically
/// `tt_shared::message_text_for_estimation(req).to_lowercase()`) so the fold
/// happens once per request, not once per keyword set.
#[must_use]
pub fn classify(text_lowercase: &str) -> Option<ReasoningWorkClass> {
    let hit = |set: &[&str]| set.iter().any(|kw| text_lowercase.contains(kw));
    if hit(MATH_KEYWORDS) {
        return Some(ReasoningWorkClass::Math);
    }
    if hit(CODE_KEYWORDS) {
        return Some(ReasoningWorkClass::Code);
    }
    if hit(LEGAL_KEYWORDS) {
        return Some(ReasoningWorkClass::Legal);
    }
    if hit(MEDICAL_KEYWORDS) {
        return Some(ReasoningWorkClass::Medical);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exemplars per class classify; generic chat / extraction prompts return
    /// `None`; matching is case-insensitive (callers lowercase once).
    #[test]
    fn reasoning_class_table() {
        let cases: &[(&str, Option<ReasoningWorkClass>)] = &[
            // Math — reasoning is the work.
            (
                "Prove that the sum of two even numbers is even.",
                Some(ReasoningWorkClass::Math),
            ),
            (
                "Solve for x: 3x + 7 = 22, then verify.",
                Some(ReasoningWorkClass::Math),
            ),
            (
                "What is the integral of x^2 dx from 0 to 1?",
                Some(ReasoningWorkClass::Math),
            ),
            (
                "Calculate the probability of drawing two aces.",
                Some(ReasoningWorkClass::Math),
            ),
            // Code.
            (
                "Refactor this Rust module to remove the unsafe block.",
                Some(ReasoningWorkClass::Code),
            ),
            (
                "Here is a stack trace from production, find the bug.",
                Some(ReasoningWorkClass::Code),
            ),
            (
                "```python\nprint('hi')\n```\nwhy does this fail?",
                Some(ReasoningWorkClass::Code),
            ),
            (
                "Implement a function that parses RFC 3339 timestamps.",
                Some(ReasoningWorkClass::Code),
            ),
            // Legal.
            (
                "Review this contract for liability exposure.",
                Some(ReasoningWorkClass::Legal),
            ),
            (
                "Which jurisdiction governs the indemnification clause?",
                Some(ReasoningWorkClass::Legal),
            ),
            // Medical.
            (
                "What dosage of amoxicillin for a 30kg patient?",
                Some(ReasoningWorkClass::Medical),
            ),
            (
                "List differential diagnoses for these symptoms.",
                Some(ReasoningWorkClass::Medical),
            ),
            // Generic chat / extraction — cappable.
            ("Summarize this article in three bullet points.", None),
            ("Translate 'good morning' into French.", None),
            ("Extract the company names from this press release.", None),
            ("Write a friendly welcome email for new users.", None),
            ("What's the capital of Australia?", None),
        ];
        for (text, want) in cases {
            let got = classify(&text.to_lowercase());
            assert_eq!(got, *want, "text {text:?} misclassified");
        }
    }

    /// Case-insensitivity is the CALLER's lowercase + our lowercase keyword
    /// sets — an upper-cased prompt classifies identically.
    #[test]
    fn classify_is_case_insensitive_via_caller_fold() {
        let shouty = "PROVE THE THEOREM HOLDS FOR ALL N.";
        assert_eq!(
            classify(&shouty.to_lowercase()),
            Some(ReasoningWorkClass::Math)
        );
    }

    /// First-match order is Math → Code → Legal → Medical (a prompt matching
    /// several sets reports the first — the order is part of the contract
    /// because the warnings token names the class).
    #[test]
    fn classify_first_match_order() {
        let mixed = "prove this function terminates"; // math + code keywords
        assert_eq!(
            classify(&mixed.to_lowercase()),
            Some(ReasoningWorkClass::Math)
        );
    }

    #[test]
    fn as_str_tokens_are_stable() {
        assert_eq!(ReasoningWorkClass::Math.as_str(), "math");
        assert_eq!(ReasoningWorkClass::Code.as_str(), "code");
        assert_eq!(ReasoningWorkClass::Legal.as_str(), "legal");
        assert_eq!(ReasoningWorkClass::Medical.as_str(), "medical");
    }
}
