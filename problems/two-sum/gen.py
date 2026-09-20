#!/usr/bin/env python3
"""Regenerates tests/03: n = 100000 with the only valid pair at the very end,
so a quadratic scan cannot exit early."""
import random

random.seed(20260920)
n = 100_000
values = [random.randint(1, 1_000_000) for _ in range(n - 2)]
values += [10**9 - 7, 10**9 - 11]
target = values[-2] + values[-1]

with open("tests/03.in", "w") as f:
    f.write(f"{n} {target}\n")
    f.write(" ".join(map(str, values)) + "\n")
with open("tests/03.out", "w") as f:
    f.write(f"{n - 2} {n - 1}\n")
