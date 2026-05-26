"""Negative: plain Python utility file with no LLM calls."""
import json

def load_config(path: str) -> dict:
    with open(path) as f:
        return json.load(f)
