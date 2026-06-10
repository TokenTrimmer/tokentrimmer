import uuid

# A per-request uuid appended to the END of the system prompt still invalidates
# the cached system block on every call.
system_prompt = f"You are a meticulous agent. Session: {uuid.uuid4()}"
