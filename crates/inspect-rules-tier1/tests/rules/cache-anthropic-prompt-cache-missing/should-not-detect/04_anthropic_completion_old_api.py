"""Negative: uses anthropic import but no messages.create call."""
import anthropic

# Using a different API, not messages.create
client = anthropic.Anthropic()
# No messages.create call here at all
print("Anthropic client created:", client)
