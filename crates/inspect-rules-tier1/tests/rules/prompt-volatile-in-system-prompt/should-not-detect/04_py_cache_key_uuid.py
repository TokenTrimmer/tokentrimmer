import uuid

# Building a cache key, not a prompt. No prompt/system/instruction context.
cache_key = f"{uuid.uuid4()}"
store[cache_key] = value
