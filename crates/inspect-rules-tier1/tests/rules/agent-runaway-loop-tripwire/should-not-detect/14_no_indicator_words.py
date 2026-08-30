"""Negative: while True loop calling ordinary functions."""
while True:
    line = sys.stdin.readline()
    if not line:
        break
    print(line.upper())
