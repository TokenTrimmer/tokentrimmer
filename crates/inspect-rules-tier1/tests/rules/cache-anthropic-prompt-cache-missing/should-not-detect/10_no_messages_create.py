"""Negative: anthropic imported but no messages.create call present."""
from anthropic import Anthropic

def build_client() -> Anthropic:
    """Return a configured Anthropic client."""
    return Anthropic()

# Note: no messages.create() call in this file.
