"""Negative: no LLM calls; pure code-based field processing."""
import re

def find_emails(text: str) -> list:
    return re.findall(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}", text)
