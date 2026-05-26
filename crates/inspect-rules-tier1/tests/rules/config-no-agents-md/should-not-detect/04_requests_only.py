"""Negative: uses requests library, not an LLM SDK."""
import requests

def get_data(url: str) -> dict:
    return requests.get(url).json()
