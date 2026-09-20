package main

import (
	"bufio"
	"fmt"
	"os"
)

func main() {
	reader := bufio.NewReader(os.Stdin)
	var a, b int64
	fmt.Fscan(reader, &a, &b)
	fmt.Println(a + b)
}
