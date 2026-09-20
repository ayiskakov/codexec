package main

import (
	"bufio"
	"fmt"
	"os"
)

func main() {
	reader := bufio.NewReaderSize(os.Stdin, 1<<20)
	var n int
	var target int64
	fmt.Fscan(reader, &n, &target)
	seen := make(map[int64]int, n)
	for j := 0; j < n; j++ {
		var value int64
		fmt.Fscan(reader, &value)
		if i, ok := seen[target-value]; ok {
			fmt.Println(i, j)
			return
		}
		seen[value] = j
	}
}
