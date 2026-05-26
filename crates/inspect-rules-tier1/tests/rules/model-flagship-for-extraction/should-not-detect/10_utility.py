"""Negative: utility functions, no LLM calls whatsoever."""
import json

def load_json(text: str) -> dict:
    return json.loads(text)
