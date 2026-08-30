"""Negative: loop with a budget circuit breaker."""
while True:
    if budget <= 0:
        break
    result = run_command(next_cmd)
    budget -= cost(result)
