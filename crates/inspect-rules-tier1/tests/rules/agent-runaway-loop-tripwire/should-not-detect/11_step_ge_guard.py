"""Negative: step >= ceiling guard in the loop."""
while True:
    if step >= 50:
        break
    run_command(next_action())
    step += 1
