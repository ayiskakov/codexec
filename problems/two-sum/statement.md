# Two Sum

Given an array of `n` integers and a `target`, find the two positions whose
values add up to `target`. Exactly one such pair exists.

## Input

- Line 1: `n` and `target` (`2 <= n <= 100000`, `|target| <= 4 * 10^9`)
- Line 2: `n` integers `a[0] .. a[n-1]` (`|a[i]| <= 2 * 10^9`)

## Output

The two 0-based indices `i j` with `i < j`.

## Example

Input:

```
4 9
2 7 11 15
```

Output:

```
0 1
```

An `O(n^2)` scan is too slow for the largest test; use a hash map.
