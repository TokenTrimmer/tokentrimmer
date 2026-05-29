import datetime
messages = [
    {"role": "system", "content": f"{datetime.datetime.utcnow()} You are a careful agent."},
    {"role": "user", "content": "Hello"},
]
