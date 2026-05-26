"""Negative: no LLM imports at all."""
import os
import json

def read_config():
    with open("config.json") as f:
        return json.load(f)
