"""Negative: infinite loop but no agent tool indicator."""
from time import sleep

def heartbeat():
    while True:
        send_health_ping()
        sleep(30)
