# User-role content carries a session id; the user turn is fresh per call and
# is not the cached prefix. No system/prompt/instruction keyword on this line.
user_turn = f"My order id is {uuid4()} and I need a refund."
