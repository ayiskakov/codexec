import sys

s = sys.stdin.readline().strip()
balanced = all(s.count(o) == s.count(c) for o, c in ("()", "[]", "{}"))
print("true" if balanced else "false")
