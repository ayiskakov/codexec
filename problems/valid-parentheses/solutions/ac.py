import sys

PAIRS = {")": "(", "]": "[", "}": "{"}


def valid(s):
    stack = []
    for ch in s:
        if ch in PAIRS:
            if not stack or stack.pop() != PAIRS[ch]:
                return False
        else:
            stack.append(ch)
    return not stack


print("true" if valid(sys.stdin.readline().strip()) else "false")
