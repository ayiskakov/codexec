import sys

line = "x" * 1023 + "\n"
for _ in range(64 * 1024):
    sys.stdout.write(line)
