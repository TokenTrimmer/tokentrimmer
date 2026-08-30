"""Positive: shell-agent loop running commands forever."""
import subprocess

def shell_agent(user_input: str):
    history = [user_input]
    while True:
        next_cmd = model(history)
        stdout = run_command(next_cmd)
        history.append(stdout)
