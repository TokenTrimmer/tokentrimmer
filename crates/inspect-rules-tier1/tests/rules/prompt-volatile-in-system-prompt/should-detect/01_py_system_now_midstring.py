import datetime

# Volatile now() embedded in the middle of the system prompt — busts the cache
# every call even though it is not the prefix.
system_prompt = f"You are a helpful assistant. The current time is {datetime.datetime.now()}. Follow the rules."
