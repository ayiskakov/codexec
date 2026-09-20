package main

import "fmt"

func main() {
	var blocks [][]byte
	for i := 0; i < 64; i++ {
		b := make([]byte, 16<<20)
		for j := range b {
			b[j] = byte(j)
		}
		blocks = append(blocks, b)
	}
	fmt.Println(len(blocks))
}
