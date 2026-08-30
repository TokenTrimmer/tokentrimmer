"""Positive: explicit step_loop naming with no iteration ceiling."""
def repl(session):
    while True:
        transcript = step_loop(session)
        session = transcript
