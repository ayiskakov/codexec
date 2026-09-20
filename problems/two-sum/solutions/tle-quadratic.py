import sys

data = sys.stdin.buffer.read().split()
n, target = int(data[0]), int(data[1])
a = list(map(int, data[2:2 + n]))
for i in range(n):
    for j in range(i + 1, n):
        if a[i] + a[j] == target:
            print(i, j)
            sys.exit(0)
