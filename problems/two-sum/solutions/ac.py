import sys


def main():
    data = sys.stdin.buffer.read().split()
    n, target = int(data[0]), int(data[1])
    seen = {}
    for j in range(n):
        value = int(data[2 + j])
        i = seen.get(target - value)
        if i is not None:
            print(i, j)
            return
        seen[value] = j


main()
