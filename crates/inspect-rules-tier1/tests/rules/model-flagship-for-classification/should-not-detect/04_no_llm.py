"""Negative: no LLM calls at all."""
def classify_rule_based(text: str) -> str:
    if "happy" in text:
        return "positive"
    return "negative"
