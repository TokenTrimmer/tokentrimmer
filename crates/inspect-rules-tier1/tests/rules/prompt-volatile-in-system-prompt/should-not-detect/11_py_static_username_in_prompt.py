# A username interpolated into a prompt is per-user but stable for that user's
# cached prefix and is not a timestamp/uuid/random token.
prompt = f"System: greet {user_name} and answer their support question helpfully."
