"""Negative: no LLM imports — rule should not fire."""
import os
import json

def load_config(path: str) -> dict:
    with open(path) as f:
        return json.load(f)
