"""Negative: while True loop but no agent/tool-call patterns."""
import time

def poll_queue():
    while True:
        item = queue.get()
        if item is None:
            break
        process(item)
        time.sleep(1)
